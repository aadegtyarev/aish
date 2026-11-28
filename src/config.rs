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
        let v = serde_norway::Value::deserialize(d)?;
        match &v {
            serde_norway::Value::Null => return Ok(Setting::Unset),
            serde_norway::Value::Bool(false) => return Ok(Setting::Off),
            serde_norway::Value::String(s) if s.eq_ignore_ascii_case("off") => {
                return Ok(Setting::Off)
            }
            _ => {}
        }
        serde_norway::from_value(v)
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
    /// `web: native` — the provider searches the web by itself; `off` — never.
    /// Unset: on for api.openai.com with `type: responses`.
    #[serde(default)]
    pub web: Setting<String>,
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
        resolve_key(self.key.as_deref()?)
    }
}

/// Literal key, or the value of `$ENV` (None if unset/empty).
pub fn resolve_key(key: &str) -> Option<String> {
    match key.strip_prefix('$') {
        Some(var) => std::env::var(var).ok().filter(|v| !v.is_empty()),
        None => Some(key.to_string()),
    }
}

/// `web:` section (tools `web_search` / `web_read`). `web: off` disables both.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct WebCfg {
    /// Backends tried in order: provider, duckduckgo, brave.
    pub search: Option<Vec<String>>,
    /// Backends tried in order: provider, direct, jina.
    pub read: Option<Vec<String>>,
    pub brave_key: Option<String>,
    pub jina_key: Option<String>,
}

pub const SEARCH_BACKENDS: &[&str] = &["provider", "duckduckgo", "brave"];
pub const READ_BACKENDS: &[&str] = &["provider", "direct", "jina"];

/// A role: one model reference, or several tried in order (fallbacks).
/// Written as `{id: ref, effort: ..}`, `{id: [a, b]}`, a bare `ref` or `[a, b]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Role {
    pub ids: Vec<String>,
    pub effort: Setting<String>,
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        fn refs<E: Error>(v: &serde_norway::Value) -> Result<Vec<String>, E> {
            let ids: Vec<String> = match v {
                serde_norway::Value::String(s) => vec![s.clone()],
                serde_norway::Value::Sequence(q) => q
                    .iter()
                    .map(|x| {
                        x.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| E::custom("model reference must be a string"))
                    })
                    .collect::<Result<_, _>>()?,
                _ => return Err(E::custom("role id must be a string or a list of strings")),
            };
            if ids.is_empty() || ids.iter().any(|s| s.trim().is_empty()) {
                return Err(E::custom("role id is empty"));
            }
            Ok(ids)
        }
        let v = serde_norway::Value::deserialize(d)?;
        match &v {
            serde_norway::Value::Mapping(m) => {
                let id = m
                    .get("id")
                    .ok_or_else(|| D::Error::custom("role needs `id`"))?;
                let effort = match m.get("effort") {
                    Some(e) => serde_norway::from_value(e.clone()).map_err(D::Error::custom)?,
                    None => Setting::Unset,
                };
                Ok(Role {
                    ids: refs(id)?,
                    effort,
                })
            }
            other => Ok(Role {
                ids: refs(other)?,
                effort: Setting::Unset,
            }),
        }
    }
}

/// `profiles:` in file order.
#[derive(Debug, Clone, Default)]
pub struct Profiles(pub Vec<(String, HashMap<String, Role>)>);

impl<'de> Deserialize<'de> for Profiles {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Profiles;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a mapping of profiles")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<Profiles, A::Error> {
                let mut v = Vec::new();
                while let Some((k, r)) = a.next_entry::<String, HashMap<String, Role>>()? {
                    v.push((k, r));
                }
                Ok(Profiles(v))
            }
        }
        d.deserialize_map(V)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub roles: HashMap<String, Role>,
    /// Named sets of roles; `profile:` picks the active one.
    #[serde(default)]
    pub profiles: Profiles,
    /// Active profile; absent: the `roles:` section.
    #[serde(default)]
    pub profile: Option<String>,
    /// Enabled modules by name; absent = all (see docs/PLUGINS.md).
    #[serde(default)]
    pub modules: Option<Vec<String>>,
    /// Name of the in-session control command (`ai` by default, `off` = none).
    #[serde(default)]
    pub command: Setting<String>,
    /// Extra command prefixes the agent may run by itself (read-only ones).
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub keys: Keys,
    #[serde(default)]
    pub web: Setting<WebCfg>,
    /// `update_check: off` — no daily check for a newer release.
    #[serde(default)]
    pub update_check: Setting<String>,
    /// The character that starts a question to the model (`?` by default).
    #[serde(default, deserialize_with = "scalar_string")]
    pub prefix: Option<String>,
    /// Conversation compaction (`compact:`).
    #[serde(default)]
    pub compact: Option<CompactCfg>,
    /// `run: ask|judge|off` — what the model's `run` tool does with commands
    /// that are not read-only (default `ask`).
    #[serde(default)]
    pub run: RunCfg,
    /// `tools: {run, files, web, skills}` — per-tool on/off.
    #[serde(default)]
    pub tools: ToolsCfg,
}

/// How the model's `run` tool treats commands that are not read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunMode {
    /// Ask before each command (default).
    #[default]
    Ask,
    /// A model decides (`run.judge`); what it does not allow asks the user.
    Judge,
    /// Never run: prefill the command into the input line.
    Off,
}

impl RunMode {
    pub const ALL: [RunMode; 3] = [RunMode::Ask, RunMode::Judge, RunMode::Off];

    pub fn name(self) -> &'static str {
        match self {
            RunMode::Ask => "ask",
            RunMode::Judge => "judge",
            RunMode::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<RunMode> {
        RunMode::ALL.into_iter().find(|m| m.name() == s)
    }
}

/// `run.judge_prompt`: text added to the default judge prompt, or replacing it
/// (a text, or a path like `~/judge.txt`).
#[derive(Debug, Clone, PartialEq)]
pub enum JudgePrompt {
    Append(String),
    Replace(String),
}

/// `run:` in the config: a scalar mode (`run: judge`), or a mapping
/// `{mode, judge, judge_prompt}`.
#[derive(Debug, Clone, Default)]
pub struct RunCfg {
    pub mode: Setting<String>,
    /// Model references of the judge (tried in order); empty: the chat role.
    pub judge: Vec<String>,
    pub judge_prompt: Option<JudgePrompt>,
}

impl<'de> Deserialize<'de> for RunCfg {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_norway::Value::deserialize(d)?;
        let mode = |v: &serde_norway::Value| -> Result<Setting<String>, D::Error> {
            serde_norway::from_value(v.clone()).map_err(D::Error::custom)
        };
        let m = match &v {
            serde_norway::Value::Mapping(m) => m,
            other => {
                return Ok(RunCfg {
                    mode: mode(other)?,
                    ..Default::default()
                })
            }
        };
        let judge = match m.get("judge") {
            None | Some(serde_norway::Value::Null) => Vec::new(),
            Some(serde_norway::Value::String(s)) => vec![s.clone()],
            Some(serde_norway::Value::Sequence(q)) => q
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| D::Error::custom("run.judge must be strings"))
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(D::Error::custom("run.judge must be a model reference")),
        };
        let judge_prompt = match m.get("judge_prompt") {
            None | Some(serde_norway::Value::Null) => None,
            Some(serde_norway::Value::Mapping(p)) => {
                let get = |k: &str| p.get(k).and_then(|x| x.as_str()).map(str::to_owned);
                match (get("append"), get("replace")) {
                    (Some(a), None) => Some(JudgePrompt::Append(a)),
                    (None, Some(r)) => Some(JudgePrompt::Replace(r)),
                    _ => {
                        return Err(D::Error::custom(
                            "run.judge_prompt: give `append` or `replace`",
                        ))
                    }
                }
            }
            Some(_) => return Err(D::Error::custom("run.judge_prompt must be a mapping")),
        };
        Ok(RunCfg {
            mode: m.get("mode").map_or(Ok(Setting::Unset), mode)?,
            judge,
            judge_prompt,
        })
    }
}

/// Switchable tools (`tools:` in the config).
pub const SWITCHABLE_TOOLS: &[&str] = &["run", "files", "web", "skills"];

/// `tools:` section: `on` / `off` per tool; absent = on. `skills` is reserved.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolsCfg {
    #[serde(default)]
    pub run: Setting<String>,
    #[serde(default)]
    pub files: Setting<String>,
    #[serde(default)]
    pub web: Setting<String>,
    #[serde(default)]
    pub skills: Setting<String>,
}

impl ToolsCfg {
    fn get(&self, name: &str) -> Option<&Setting<String>> {
        Some(match name {
            "run" => &self.run,
            "files" => &self.files,
            "web" => &self.web,
            "skills" => &self.skills,
            _ => return None,
        })
    }

    /// Is the tool on (unset = on)?
    pub fn on(&self, name: &str) -> bool {
        !matches!(self.get(name), Some(Setting::Off))
    }
}

/// Any scalar read as text (`prefix: 5` must reach our own message).
fn scalar_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match serde_norway::Value::deserialize(d)? {
        serde_norway::Value::String(s) => Some(s),
        serde_norway::Value::Number(n) => Some(n.to_string()),
        serde_norway::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

/// A share written as `70%` (or a bare `70`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Percent(pub f64);

impl<'de> Deserialize<'de> for Percent {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = serde_norway::Value::deserialize(d)?;
        let n = match &v {
            serde_norway::Value::Number(n) => n.as_f64(),
            serde_norway::Value::String(s) => s.trim().trim_end_matches('%').trim().parse().ok(),
            _ => None,
        };
        match n {
            Some(n) if n.is_finite() => Ok(Percent(n)),
            _ => Err(D::Error::custom("expected a percentage like 70%")),
        }
    }
}

/// `compact:` — when and how the conversation is summarised.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompactCfg {
    /// Model reference for the summary; default: the chat model in use.
    pub model: Option<String>,
    /// Compact when the conversation exceeds this share of the window.
    pub at: Option<Percent>,
    /// Target summary size as a share of the window.
    pub size: Option<Percent>,
    pub prompt: Option<String>,
}

pub const COMPACT_AT: f64 = 70.0;
pub const COMPACT_SIZE: f64 = 10.0;

/// Key bindings (`keys:` in the config).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Keys {
    /// `ctrl-<letter>` or `off`; asks the model from anywhere (default ctrl-q).
    #[serde(default)]
    pub ask: Setting<String>,
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
        let cfg: Config = serde_norway::from_str(src).context("invalid config YAML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        for (i, p) in self.providers.iter().enumerate() {
            if self.providers[..i].iter().any(|q| q.id == p.id) {
                bail!("duplicate provider id '{}'", p.id);
            }
        }
        for r in &self.run.judge {
            self.resolve_ref(r, &Setting::Unset)
                .map_err(|e| anyhow!("run.judge: {e}"))?;
        }
        for name in SWITCHABLE_TOOLS {
            if let Some(Setting::Value(v)) = self.tools.get(name) {
                if v != "on" {
                    bail!("tools.{name}: must be on or off");
                }
            }
        }
        if let Setting::Value(w) = &self.web {
            for (chain, known, what) in [
                (&w.search, SEARCH_BACKENDS, "search"),
                (&w.read, READ_BACKENDS, "read"),
            ] {
                for b in chain.iter().flatten() {
                    if !known.contains(&b.to_ascii_lowercase().as_str()) {
                        bail!(
                            "web.{what}: unknown backend '{b}' (known: {})",
                            known.join(", ")
                        );
                    }
                }
            }
        }
        for p in &self.providers {
            for (alias, m) in &p.models {
                if let Setting::Value(w) = &m.context_window {
                    if *w < 2048 {
                        bail!(
                            "model '{}/{alias}': context_window must be at least 2048",
                            p.id
                        );
                    }
                }
                if let Setting::Value(v) = &m.web {
                    if v != "native" {
                        bail!("model '{}/{alias}': web must be 'native' or off", p.id);
                    }
                }
            }
        }
        if let Some(p) = &self.prefix {
            crate::llm::prefix::from_config(p).map_err(|e| anyhow!(e))?;
        }
        if let Some(c) = &self.compact {
            let at = c.at.map_or(COMPACT_AT, |p| p.0);
            let size = c.size.map_or(COMPACT_SIZE, |p| p.0);
            if !(10.0..=90.0).contains(&at) {
                bail!("compact.at must be between 10% and 90%");
            }
            if !(1.0..=50.0).contains(&size) || size >= at {
                bail!("compact.size must be between 1% and 50%, and below compact.at");
            }
            if let Some(m) = &c.model {
                self.resolve_ref(m, &Setting::Unset)
                    .map_err(|e| anyhow!("compact.model: {e}"))?;
            }
        }
        for (i, (n, _)) in self.profiles.0.iter().enumerate() {
            if self.profiles.0[..i].iter().any(|(m, _)| m == n) {
                bail!("duplicate profile '{n}'");
            }
        }
        if let Some(p) = &self.profile {
            if self.profile_roles(p).is_none() {
                bail!("profile '{p}' is not in `profiles:`");
            }
        }
        for name in self.roles.keys() {
            self.resolve_chain(name, None)?;
        }
        for (pn, roles) in &self.profiles.0 {
            for name in roles.keys() {
                self.resolve_chain(name, Some(pn))
                    .map_err(|e| anyhow!("profile '{pn}': {e}"))?;
            }
        }
        Ok(())
    }

    /// The question prefix (`?` unless configured).
    pub fn prefix(&self) -> char {
        self.prefix
            .as_deref()
            .and_then(|p| crate::llm::prefix::from_config(p).ok())
            .unwrap_or('?')
    }

    /// Share of the window at which the conversation is compacted (0..1).
    pub fn compact_at(&self) -> f64 {
        self.compact
            .as_ref()
            .and_then(|c| c.at)
            .map_or(COMPACT_AT, |p| p.0)
            .min(90.0)
            / 100.0
    }

    /// Target summary size as a share of the window (0..1).
    pub fn compact_size(&self) -> f64 {
        self.compact
            .as_ref()
            .and_then(|c| c.size)
            .map_or(COMPACT_SIZE, |p| p.0)
            / 100.0
    }

    /// The configured `run:` mode (`ask` when unset).
    pub fn run_mode(&self) -> RunMode {
        match &self.run.mode {
            Setting::Off => RunMode::Off,
            Setting::Value(v) => RunMode::parse(v).unwrap_or_default(),
            Setting::Unset => RunMode::Ask,
        }
    }

    pub fn profile_roles(&self, name: &str) -> Option<&HashMap<String, Role>> {
        self.profiles
            .0
            .iter()
            .find(|(n, _)| n == name)
            .map(|x| &x.1)
    }

    /// The role as configured now: the active profile's entry, else `roles:`.
    pub fn role(&self, role: &str) -> Option<&Role> {
        self.role_in(role, self.profile.as_deref())
    }

    fn role_in(&self, role: &str, profile: Option<&str>) -> Option<&Role> {
        profile
            .and_then(|p| self.profile_roles(p))
            .and_then(|r| r.get(role))
            .or_else(|| self.roles.get(role))
    }

    fn resolve_chain(&self, role: &str, profile: Option<&str>) -> Result<Vec<Resolved<'_>>> {
        let r = self
            .role_in(role, profile)
            .ok_or_else(|| anyhow!("unknown role '{role}'"))?;
        r.ids
            .iter()
            .map(|id| {
                self.resolve_ref(id, &r.effort)
                    .map_err(|e| anyhow!("role '{role}': {e}"))
            })
            .collect()
    }

    /// Every model of the role in fallback order.
    pub fn resolve_all(&self, role: &str) -> Result<Vec<Resolved<'_>>> {
        self.resolve_chain(role, self.profile.as_deref())
    }

    pub fn resolve(&self, role: &str) -> Result<Resolved<'_>> {
        Ok(self.resolve_all(role)?.remove(0))
    }

    /// Providers that have a model with this alias.
    pub fn providers_with(&self, alias: &str) -> Vec<&str> {
        self.providers
            .iter()
            .filter(|p| p.models.contains_key(alias))
            .map(|p| p.id.as_str())
            .collect()
    }

    /// Resolve a `provider/alias` reference, or a bare `alias` that only one
    /// provider has; `effort` is the role-level setting.
    pub fn resolve_ref(&self, id: &str, effort: &Setting<String>) -> Result<Resolved<'_>> {
        let (pid, alias) = match id.split_once('/') {
            Some(x) => x,
            None => match self.providers_with(id).as_slice() {
                [p] => (*p, id),
                [] => return Err(anyhow!("no model alias '{id}'")),
                ps => {
                    return Err(anyhow!(
                        "alias '{id}' is in several providers ({}): use provider/{id}",
                        ps.join(", ")
                    ))
                }
            },
        };
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
    Some(base.join("aish/config.yaml"))
}

/// Missing file is fine (`Ok(None)`); a present but broken file is an error.
pub fn load() -> Result<Option<Config>> {
    match config_path() {
        Some(p) => load_from(&p),
        None => Ok(None),
    }
}

pub fn load_from(path: &std::path::Path) -> Result<Option<Config>> {
    match std::fs::read_to_string(path) {
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
    fn bare_alias_resolves_when_unique() {
        let c = Config::parse(
            "providers:\n  - id: a\n    url: http://x/v1\n    type: chat\n    models: {m: {id: m1}, both: {id: b1}}\n  - id: b\n    url: http://y/v1\n    type: chat\n    models: {both: {id: b2}}\nroles:\n  chat: {id: m}\n",
        )
        .unwrap();
        assert_eq!(c.resolve("chat").unwrap().model.id, "m1");
        assert_eq!(
            c.resolve_ref("b/both", &Setting::Unset).unwrap().model.id,
            "b2"
        );
        let e = c
            .resolve_ref("both", &Setting::Unset)
            .unwrap_err()
            .to_string();
        assert!(e.contains("a, b"), "{e}");
        assert!(c.resolve_ref("nope", &Setting::Unset).is_err());
    }

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
            ("x: {id: pm}", "no model alias 'pm'"),
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
        std::env::set_var("AISH_TEST_KEY", "secret");
        p.key = Some("$AISH_TEST_KEY".into());
        assert_eq!(p.api_key().as_deref(), Some("secret"));
        p.key = Some("lit".into());
        assert_eq!(p.api_key().as_deref(), Some("lit"));
        p.key = Some("$AISH_TEST_UNSET".into());
        assert_eq!(p.api_key(), None);
    }

    #[test]
    fn keys_section() {
        let c = Config::parse("keys: {ask: ctrl-g}").unwrap();
        assert_eq!(c.keys.ask, Setting::Value("ctrl-g".into()));
        assert_eq!(
            Config::parse("keys: {ask: off}").unwrap().keys.ask,
            Setting::Off
        );
        assert_eq!(Config::parse("{}").unwrap().keys.ask, Setting::Unset);
    }

    #[test]
    fn web_section() {
        let c = Config::parse("web: {search: [duckduckgo, brave], brave_key: $K}").unwrap();
        let Setting::Value(w) = &c.web else { panic!() };
        assert_eq!(w.search.as_deref().unwrap().len(), 2);
        assert!(w.read.is_none());
        assert_eq!(Config::parse("web: off").unwrap().web, Setting::Off);
        assert!(matches!(Config::parse("{}").unwrap().web, Setting::Unset));
        assert!(Config::parse("web: {search: [bing]}").is_err());
        let m = "providers:\n  - {id: p, url: u, type: chat, models: {m: {id: a, web: native}}}";
        assert!(Config::parse(m).is_ok());
        assert!(Config::parse(&m.replace("native", "yes")).is_err());
    }

    #[test]
    fn run_and_tools_sections() {
        let c = Config::parse("{}").unwrap();
        assert_eq!(c.run_mode(), RunMode::Ask);
        assert!(c.tools.on("run") && c.tools.on("files") && c.tools.on("web"));
        let j = Config::parse("providers: [{id: p, url: u, type: chat, models: {m: {id: a}}}]\nrun: {mode: judge, judge: [p/m], judge_prompt: {append: be strict}}").unwrap();
        assert_eq!(j.run_mode(), RunMode::Judge);
        assert_eq!(j.run.judge, ["p/m"]);
        assert_eq!(
            j.run.judge_prompt,
            Some(JudgePrompt::Append("be strict".into()))
        );
        assert!(Config::parse("run: {mode: judge, judge: nope/x}").is_err());
        assert!(Config::parse("run: {judge_prompt: {append: a, replace: b}}").is_err());
        assert_eq!(
            Config::parse("run: {mode: off}").unwrap().run_mode(),
            RunMode::Off
        );
        let c = Config::parse("run: judge\ntools: {web: off, run: on}").unwrap();
        assert_eq!(c.run_mode(), RunMode::Judge);
        assert!(c.tools.on("run") && !c.tools.on("web"));
        assert_eq!(Config::parse("run: off").unwrap().run_mode(), RunMode::Off);
        // unknown (also the unreleased `auto`): ask
        assert_eq!(
            Config::parse("run: sometimes").unwrap().run_mode(),
            RunMode::Ask
        );
        assert_eq!(Config::parse("run: auto").unwrap().run_mode(), RunMode::Ask);
        assert!(Config::parse("tools: {web: maybe}").is_err());
        assert!(!Config::parse("tools: {skills: off}")
            .unwrap()
            .tools
            .on("skills"));
    }

    #[test]
    fn empty_config_ok() {
        assert!(Config::parse("{}").unwrap().providers.is_empty());
    }

    #[test]
    fn prefix_and_compact_validation() {
        let p = |x: &str| Config::parse(x);
        assert_eq!(p("").unwrap().prefix(), '?');
        for c in ["?", ",", "@", "#", ";"] {
            assert_eq!(
                p(&format!("prefix: '{c}'")).unwrap().prefix(),
                c.chars().next().unwrap()
            );
        }
        for bad in ["'.'", "'??'", "''", "'!'", "5"] {
            let e = p(&format!("prefix: {bad}")).unwrap_err().to_string();
            assert!(e.contains("? , @ # ;"), "{bad}: {e}");
        }
        let c = p("compact: {at: 80%, size: 5%}").unwrap();
        assert!((c.compact_at() - 0.8).abs() < 1e-9 && (c.compact_size() - 0.05).abs() < 1e-9);
        let c = p("").unwrap();
        assert!((c.compact_at() - 0.7).abs() < 1e-9 && (c.compact_size() - 0.1).abs() < 1e-9);
        assert!(p("compact: {at: 99%}").is_err());
        assert!(p("compact: {at: 91%}").is_err() && p("compact: {at: 90%}").is_ok());
        let w = |n: u32| {
            p(&format!(
                "providers:\n  - id: a\n    url: http://x/v1\n    type: chat\n    models: {{m: {{id: m1, context_window: {n}}}}}\n"
            ))
        };
        assert!(w(1000).unwrap_err().to_string().contains("2048"));
        assert!(w(2048).is_ok());
        assert!(p("compact: {at: 20%, size: 30%}").is_err());
        assert!(p("compact: {at: lots}").is_err());
        assert!(p("compact: {model: nope}").is_err());
    }
}
