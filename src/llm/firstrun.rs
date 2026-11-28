//! First start without a config: look for local servers and an OpenAI key,
//! write a config and tell the user once. Runs on a worker thread.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use std::time::Duration;

use super::Llm;

/// Default cloud chat model written into a new config.
pub const DEFAULT_CLOUD_MODEL: &str = "gpt-5.5";
const OPENAI_URL: &str = "https://api.openai.com/v1";
// Runs in the background and the line may arrive late, so this can be generous.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

pub struct Found {
    pub id: &'static str,
    pub url: String,
    pub models: Vec<String>,
}

/// Rough parameter count (billions) from a model name like `qwen3:32b`.
pub fn size_b(name: &str) -> Option<f64> {
    let l = name.to_ascii_lowercase();
    let b = l.as_bytes();
    let mut best = None;
    for i in 0..b.len() {
        if !b[i].is_ascii_digit() || (i > 0 && (b[i - 1].is_ascii_digit() || b[i - 1] == b'.')) {
            continue;
        }
        let mut j = i;
        while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.') {
            j += 1;
        }
        let unit = b.get(j).copied();
        let after = b.get(j + 1).copied();
        if matches!(unit, Some(b'b' | b'm')) && !after.is_some_and(|c| c.is_ascii_alphabetic()) {
            if let Ok(v) = l[i..j].parse::<f64>() {
                best = Some(if unit == Some(b'm') { v / 1000.0 } else { v });
            }
        }
    }
    best
}

fn is_embedding(n: &str) -> bool {
    let l = n.to_ascii_lowercase();
    l.contains("embed") || l.contains("rerank")
}

fn alias(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[derive(Serialize)]
struct CfgModel {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
}

#[derive(Serialize)]
struct CfgProvider {
    id: String,
    url: String,
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    models: BTreeMap<String, CfgModel>,
}

#[derive(Serialize)]
struct CfgRole {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
}

#[derive(Serialize)]
struct Cfg {
    providers: Vec<CfgProvider>,
    roles: BTreeMap<&'static str, CfgRole>,
}

/// Unique config alias for each model id of one provider (`a:b` -> `a-b`, `a-b-2`...).
fn aliases(models: &[&String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for m in models {
        let base = alias(m);
        let mut a = base.clone();
        let mut n = 2;
        while seen.contains(&a) {
            a = format!("{base}-{n}");
            n += 1;
        }
        seen.push(a);
    }
    seen
}

const DEEPSEEK_URL: &str = "https://api.deepseek.com/v1";

/// alias -> model for each (model id, alias) pair.
fn model_map(ms: &[&String], aliases: &[String]) -> BTreeMap<String, CfgModel> {
    ms.iter()
        .zip(aliases)
        .map(|(m, a)| {
            let id = (*m).clone();
            (a.clone(), CfgModel { id, effort: None })
        })
        .collect()
}

/// Pick roles and render the config, optionally with a DeepSeek provider
/// (`$DEEPSEEK_API_KEY`) whose model ids were listed from `/models`.
/// Returns `(yaml, chat, complete)` labels.
pub fn build_with(
    found: &[Found],
    key: bool,
    cloud_url: &str,
    deepseek: Option<(&str, &[String])>,
) -> Option<(String, String, String)> {
    // (provider, model id, alias)
    let mut locals: Vec<(&Found, &String, String)> = Vec::new();
    let mut providers = Vec::new();
    for f in found {
        let ms: Vec<&String> = f
            .models
            .iter()
            .filter(|m| !is_embedding(m))
            .take(30)
            .collect();
        if ms.is_empty() {
            continue;
        }
        let al = aliases(&ms);
        for (m, a) in ms.iter().zip(&al) {
            locals.push((f, m, a.clone()));
        }
        providers.push(CfgProvider {
            id: f.id.to_owned(),
            url: f.url.clone(),
            kind: "chat",
            key: None,
            models: model_map(&ms, &al),
        });
    }
    let ds_models: Vec<&String> = deepseek
        .map(|(_, m)| m.iter().filter(|m| !is_embedding(m)).take(10).collect())
        .unwrap_or_default();
    if locals.is_empty() && !key && ds_models.is_empty() {
        return None;
    }
    let size_key = |m: &String| {
        (
            !m.to_ascii_lowercase().contains("coder"),
            size_b(m).unwrap_or(1e9),
        )
    };
    let complete = locals.iter().min_by(|a, b| {
        let (ka, kb) = (size_key(a.1), size_key(b.1));
        ka.0.cmp(&kb.0)
            .then(ka.1.partial_cmp(&kb.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    let big = locals.iter().max_by(|a, b| {
        let k = |m: &String| size_b(m).unwrap_or(-1.0);
        k(a.1)
            .partial_cmp(&k(b.1))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if key {
        providers.push(CfgProvider {
            id: "cloud".into(),
            url: cloud_url.to_owned(),
            kind: "responses",
            key: Some("$OPENAI_API_KEY".into()),
            models: BTreeMap::from([(
                "main".to_owned(),
                CfgModel {
                    id: DEFAULT_CLOUD_MODEL.into(),
                    effort: Some("medium".into()),
                },
            )]),
        });
    }
    // (alias, model id) of the DeepSeek chat model: `flash`, else `chat`, else the first
    let mut ds_pick: Option<(String, String)> = None;
    if let (Some((url, _)), false) = (deepseek, ds_models.is_empty()) {
        let al = aliases(&ds_models);
        let pick = ds_models
            .iter()
            .position(|m| m.contains("flash"))
            .or_else(|| ds_models.iter().position(|m| m.contains("chat")))
            .unwrap_or(0);
        ds_pick = Some((al[pick].clone(), ds_models[pick].clone()));
        providers.push(CfgProvider {
            id: "deepseek".into(),
            url: url.to_owned(),
            kind: "chat",
            key: Some("$DEEPSEEK_API_KEY".into()),
            models: model_map(&ds_models, &al),
        });
    }
    let mut roles = BTreeMap::new();
    let mut chat = "—".to_owned();
    let mut comp = "—".to_owned();
    if let Some((f, m, a)) = complete {
        roles.insert(
            "complete",
            CfgRole {
                id: format!("{}/{a}", f.id),
                effort: None,
            },
        );
        comp = format!("{}/{m}", f.id);
    }
    // (role id, effort, label) of the chat model: cloud, else DeepSeek, else the biggest local
    let pick = if key {
        Some((
            "cloud/main".to_owned(),
            Some("low"),
            format!("cloud/{DEFAULT_CLOUD_MODEL}"),
        ))
    } else if let Some((a, m)) = ds_pick {
        Some((format!("deepseek/{a}"), None, format!("deepseek/{m}")))
    } else {
        big.map(|(f, m, a)| (format!("{}/{a}", f.id), None, format!("{}/{m}", f.id)))
    };
    if let Some((id, effort, label)) = pick {
        roles.insert(
            "chat",
            CfgRole {
                id,
                effort: effort.map(Into::into),
            },
        );
        chat = label;
    }
    let yaml = serde_norway::to_string(&Cfg { providers, roles }).ok()?;
    Some((yaml, chat, comp))
}

/// Start the detection in the background if there is no config yet.
pub fn spawn(llm: Arc<Llm>) {
    if llm.path().is_none() || llm.config_exists() {
        return;
    }
    std::thread::spawn(move || {
        if let Err(e) = run(&llm) {
            tracing::warn!("first run: {e}");
        }
    });
}

/// What the machine offers: local servers, `$OPENAI_API_KEY`, `$DEEPSEEK_API_KEY`.
pub struct Detected {
    pub found: Vec<Found>,
    pub openai: bool,
    /// DeepSeek endpoint and its model ids (key present and `/models` answered).
    pub deepseek: Option<(String, Vec<String>)>,
}

impl Detected {
    /// The config these findings make: `(yaml, chat, complete)`.
    pub fn config(&self) -> Option<(String, String, String)> {
        build_with(
            &self.found,
            self.openai,
            &env_or("AISH_OPENAI_URL", OPENAI_URL),
            self.deepseek
                .as_ref()
                .map(|(u, m)| (u.as_str(), m.as_slice())),
        )
    }

    /// One-line list for messages: `ollama, deepseek`.
    pub fn names(&self) -> String {
        let mut v: Vec<&str> = self.found.iter().map(|f| f.id).collect();
        if self.openai {
            v.push("openai");
        }
        if self.deepseek.is_some() {
            v.push("deepseek");
        }
        v.join(", ")
    }
}

pub fn detect() -> Detected {
    let urls = [
        (
            "ollama",
            env_or("AISH_PROBE_OLLAMA", "http://localhost:11434/v1"),
        ),
        (
            "lmstudio",
            env_or("AISH_PROBE_LMSTUDIO", "http://localhost:1234/v1"),
        ),
        (
            "llamacpp",
            env_or("AISH_PROBE_LLAMACPP", "http://localhost:8080/v1"),
        ),
    ];
    let hs: Vec<_> = urls
        .into_iter()
        .map(|(id, url)| {
            std::thread::spawn(move || {
                aish_llm::list_models(&url, &None, PROBE_TIMEOUT)
                    .ok()
                    .map(|models| Found { id, url, models })
            })
        })
        .collect();
    let found: Vec<Found> = hs
        .into_iter()
        .filter_map(|h| h.join().ok().flatten())
        .collect();
    let openai = std::env::var("OPENAI_API_KEY").is_ok_and(|k| !k.is_empty());
    let deepseek = std::env::var("DEEPSEEK_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .and_then(|k| {
            let url = env_or("AISH_DEEPSEEK_URL", DEEPSEEK_URL);
            let models = aish_llm::list_models(&url, &Some(k), Duration::from_secs(3)).ok()?;
            Some((url, models))
        });
    Detected {
        found,
        openai,
        deepseek,
    }
}

fn run(llm: &Llm) -> std::io::Result<()> {
    let Some((yaml, chat, comp)) = detect().config() else {
        return Ok(());
    };
    let path = llm.path().expect("checked");
    if !crate::fsio::write_new(path, &yaml)? {
        return Ok(()); // someone else created it first
    }
    llm.set_notice(format!(
        "aish · chat: {chat} · complete: {comp} · aish config — change"
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn f(id: &'static str, m: &[&str]) -> Found {
        Found {
            id,
            url: "http://localhost:1/v1".into(),
            models: m.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(size_b("qwen2.5-coder:1.5b"), Some(1.5));
        assert_eq!(size_b("qwen3:32b"), Some(32.0));
        assert_eq!(size_b("llama3.1:8b-instruct-q4"), Some(8.0));
        assert_eq!(size_b("gemma-270m"), Some(0.27));
        assert_eq!(size_b("mistral"), None);
    }

    #[test]
    fn local_only_picks_coder_and_largest_and_parses() {
        let found = [f(
            "ollama",
            &[
                "qwen3:32b",
                "qwen2.5-coder:7b",
                "qwen2.5-coder:1.5b",
                "nomic-embed-text",
            ],
        )];
        let (y, chat, comp) = build_with(&found, false, "u", None).unwrap();
        assert_eq!(comp, "ollama/qwen2.5-coder:1.5b");
        assert_eq!(chat, "ollama/qwen3:32b");
        let c = Config::parse(&y).unwrap();
        assert_eq!(
            c.resolve("complete").unwrap().model.id,
            "qwen2.5-coder:1.5b"
        );
        assert_eq!(c.resolve("chat").unwrap().model.id, "qwen3:32b");
    }

    #[test]
    fn colliding_aliases_are_made_unique() {
        let found = [f("ollama", &["a:b", "a-b", "a/b"])];
        let (y, _, _) = build_with(&found, false, "u", None).unwrap();
        let c = Config::parse(&y).unwrap();
        assert_eq!(c.providers[0].models.len(), 3, "{y}");
    }

    #[test]
    fn write_new_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/config.yaml");
        assert!(crate::fsio::write_new(&p, "one").unwrap());
        assert!(!crate::fsio::write_new(&p, "two").unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one");
        // no temp files left behind
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn key_makes_cloud_chat() {
        let found = [f("lmstudio", &["small-3b"])];
        let (y, chat, comp) = build_with(&found, true, "https://x/v1", None).unwrap();
        assert_eq!(chat, format!("cloud/{DEFAULT_CLOUD_MODEL}"));
        assert_eq!(comp, "lmstudio/small-3b");
        let c = Config::parse(&y).unwrap();
        assert_eq!(c.resolve("chat").unwrap().provider.id, "cloud");
        assert!(build_with(&[], false, "u", None).is_none());
        let (y, _, comp) = build_with(&[], true, "u", None).unwrap();
        assert_eq!(comp, "—");
        Config::parse(&y).unwrap();
    }
}
