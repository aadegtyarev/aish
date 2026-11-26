//! YAML config (see docs/config.example.yaml) and role resolution.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

/// A value that may be unset, explicitly `off`, or a concrete value.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Setting<T> {
    #[default]
    Unset,
    Off,
    Value(T),
}

impl<T> Setting<T> {
    /// `off` and unset both mean "none".
    pub fn value(self) -> Option<T> {
        match self {
            Setting::Value(v) => Some(v),
            _ => None,
        }
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for Setting<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_yaml::Value::deserialize(d)?;
        match &v {
            serde_yaml::Value::Null => return Ok(Setting::Unset),
            serde_yaml::Value::Bool(false) => return Ok(Setting::Off),
            serde_yaml::Value::String(s) if s.eq_ignore_ascii_case("off") => {
                return Ok(Setting::Off)
            }
            _ => {}
        }
        serde_yaml::from_value(v)
            .map(Setting::Value)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    Chat,
    Responses,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub effort: Setting<String>,
    #[serde(default)]
    pub context_window: Setting<u64>,
    /// `tools: off` — never offer function tools to this model.
    #[serde(default)]
    pub tools: Setting<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    pub id: String,
    pub url: String,
    #[serde(rename = "type")]
    pub kind: ProviderType,
    pub key: Option<String>,
    #[serde(default)]
    pub models: HashMap<String, Model>,
}

impl Provider {
    /// Literal key, or the value of `$ENV` (None if unset/empty).
    pub fn api_key(&self) -> Option<String> {
        let key = self.key.as_deref()?;
        match key.strip_prefix('$') {
            Some(var) => std::env::var(var).ok().filter(|v| !v.is_empty()),
            None => Some(key.to_string()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Role {
    pub id: String,
    #[serde(default)]
    pub effort: Setting<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub roles: HashMap<String, Role>,
    /// Enabled modules by name; absent = all (see docs/PLUGINS.md).
    #[serde(default)]
    pub modules: Option<Vec<String>>,
    /// Name of the in-session control command (`ai` by default, `off` = none).
    #[serde(default)]
    pub command: Setting<String>,
    /// Extra command prefixes the agent may run by itself (read-only ones).
    #[serde(default)]
    pub allow: Vec<String>,
}

/// A role resolved to concrete provider/model/effort.
#[derive(Debug, Clone)]
pub struct Resolved<'a> {
    pub provider: &'a Provider,
    pub alias: &'a str,
    pub model: &'a Model,
    /// Role effort overrides model effort; `off` on either level means none.
    pub effort: Option<String>,
}

impl Config {
    pub fn parse(src: &str) -> Result<Config> {
        let cfg: Config = serde_yaml::from_str(src).context("invalid config YAML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        for (i, p) in self.providers.iter().enumerate() {
            if self.providers[..i].iter().any(|q| q.id == p.id) {
                bail!("duplicate provider id '{}'", p.id);
            }
        }
        for name in self.roles.keys() {
            self.resolve(name)?;
        }
        Ok(())
    }

    pub fn resolve(&self, role: &str) -> Result<Resolved<'_>> {
        let r = self
            .roles
            .get(role)
            .ok_or_else(|| anyhow!("unknown role '{role}'"))?;
        self.resolve_ref(&r.id, &r.effort)
            .map_err(|e| anyhow!("role '{role}': {e}"))
    }

    /// Resolve a `provider/alias` reference; `effort` is the role-level setting.
    pub fn resolve_ref(&self, id: &str, effort: &Setting<String>) -> Result<Resolved<'_>> {
        let (pid, alias) = id
            .split_once('/')
            .ok_or_else(|| anyhow!("id '{id}' must be 'provider/alias'"))?;
        let provider = self
            .providers
            .iter()
            .find(|p| p.id == pid)
            .ok_or_else(|| anyhow!("unknown provider '{pid}'"))?;
        let (alias, model) = provider
            .models
            .get_key_value(alias)
            .ok_or_else(|| anyhow!("provider '{pid}' has no model alias '{alias}'"))?;
        let effort = match effort {
            Setting::Unset => model.effort.clone().value(),
            e => e.clone().value(),
        };
        Ok(Resolved {
            provider,
            alias,
            model,
            effort,
        })
    }
}

pub fn config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("llmsh/config.yaml"))
}

/// Missing file is fine (`Ok(None)`); a present but broken file is an error.
pub fn load() -> Result<Option<Config>> {
    let Some(path) = config_path() else {
        return Ok(None);
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => Config::parse(&s)
            .with_context(|| path.display().to_string())
            .map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../docs/config.example.yaml");

    #[test]
    fn parses_example_and_resolves() {
        let c = Config::parse(EXAMPLE).unwrap();
        assert_eq!(c.providers.len(), 2);
        let chat = c.resolve("chat").unwrap();
        assert_eq!(chat.provider.id, "cloud");
        assert_eq!(chat.provider.kind, ProviderType::Responses);
        assert_eq!(chat.model.id, "gpt-6");
        assert_eq!(chat.effort.as_deref(), Some("low")); // role overrides model
        assert_eq!(chat.model.context_window, Setting::Value(400000));
        let complete = c.resolve("complete").unwrap();
        assert_eq!(complete.alias, "coder");
        assert_eq!(complete.effort, None);
    }

    #[test]
    fn model_effort_inherited_and_off() {
        let y = "
providers:
  - id: p
    url: http://x
    type: chat
    models:
      m: {id: a, effort: medium, context_window: off}
roles:
  a: {id: p/m}
  b: {id: p/m, effort: off}
";
        let c = Config::parse(y).unwrap();
        assert_eq!(c.resolve("a").unwrap().effort.as_deref(), Some("medium"));
        assert_eq!(c.resolve("b").unwrap().effort, None);
        assert_eq!(c.providers[0].models["m"].context_window, Setting::Off);
    }

    #[test]
    fn bad_refs() {
        let base = "providers:\n  - {id: p, url: u, type: chat, models: {m: {id: a}}}\nroles:\n";
        for (role, want) in [
            ("x: {id: pm}", "provider/alias"),
            ("x: {id: q/m}", "unknown provider 'q'"),
            ("x: {id: p/z}", "no model alias 'z'"),
        ] {
            let err = Config::parse(&format!("{base}  {role}")).unwrap_err();
            assert!(format!("{err:#}").contains(want), "{err:#}");
        }
        assert!(Config::parse("providers: [{id: p, url: u, type: nope}]").is_err());
    }

    #[test]
    fn api_key_env_and_literal() {
        let mut p = Config::parse(EXAMPLE).unwrap().providers.remove(1);
        std::env::set_var("LLMSH_TEST_KEY", "secret");
        p.key = Some("$LLMSH_TEST_KEY".into());
        assert_eq!(p.api_key().as_deref(), Some("secret"));
        p.key = Some("lit".into());
        assert_eq!(p.api_key().as_deref(), Some("lit"));
        p.key = Some("$LLMSH_TEST_UNSET".into());
        assert_eq!(p.api_key(), None);
    }

    #[test]
    fn empty_config_ok() {
        assert!(Config::parse("{}").unwrap().providers.is_empty());
    }
}
