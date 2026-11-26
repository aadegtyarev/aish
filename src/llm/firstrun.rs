//! First start without a config: look for local servers and an OpenAI key,
//! write a config and tell the user once. Runs on a worker thread.

use std::sync::Arc;
use std::time::Duration;

use super::Llm;

/// Default cloud chat model written into a new config.
pub const DEFAULT_CLOUD_MODEL: &str = "gpt-5.5";
const OPENAI_URL: &str = "https://api.openai.com/v1";
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

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

/// Pick roles and render the config. Returns `(yaml, chat, complete)` labels.
pub fn build(found: &[Found], key: bool, cloud_url: &str) -> Option<(String, String, String)> {
    let locals: Vec<(&Found, &String)> = found
        .iter()
        .flat_map(|f| f.models.iter().map(move |m| (f, m)))
        .filter(|(_, m)| !is_embedding(m))
        .collect();
    let complete = locals
        .iter()
        .min_by(|a, b| {
            let k = |m: &String| {
                (
                    !m.to_ascii_lowercase().contains("coder"),
                    size_b(m).unwrap_or(1e9),
                )
            };
            let (ka, kb) = (k(a.1), k(b.1));
            ka.0.cmp(&kb.0)
                .then(ka.1.partial_cmp(&kb.1).unwrap_or(std::cmp::Ordering::Equal))
        })
        .copied();
    let big = locals
        .iter()
        .max_by(|a, b| {
            let k = |m: &String| size_b(m).unwrap_or(-1.0);
            k(a.1)
                .partial_cmp(&k(b.1))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied();
    if locals.is_empty() && !key {
        return None;
    }
    let mut y = String::from("providers:\n");
    for f in found.iter().filter(|f| !f.models.is_empty()) {
        y.push_str(&format!(
            "  - id: {}\n    url: {}\n    type: chat\n    models:\n",
            f.id, f.url
        ));
        for m in f.models.iter().filter(|m| !is_embedding(m)).take(30) {
            y.push_str(&format!("      {}: {{id: \"{m}\"}}\n", alias(m)));
        }
    }
    if key {
        y.push_str(&format!(
            "  - id: cloud\n    url: {cloud_url}\n    type: responses\n    key: $OPENAI_API_KEY\n    models:\n      main:\n        id: {DEFAULT_CLOUD_MODEL}\n        effort: medium\n"
        ));
    }
    y.push_str("\nroles:\n");
    let mut chat = "—".to_owned();
    let mut comp = "—".to_owned();
    if let Some((f, m)) = complete {
        y.push_str(&format!("  complete:\n    id: {}/{}\n", f.id, alias(m)));
        comp = format!("{}/{m}", f.id);
    }
    if key {
        y.push_str("  chat:\n    id: cloud/main\n    effort: low\n");
        chat = format!("cloud/{DEFAULT_CLOUD_MODEL}");
    } else if let Some((f, m)) = big {
        y.push_str(&format!("  chat:\n    id: {}/{}\n", f.id, alias(m)));
        chat = format!("{}/{m}", f.id);
    }
    Some((y, chat, comp))
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

fn run(llm: &Llm) -> std::io::Result<()> {
    let urls = [
        (
            "ollama",
            env_or("LLMSH_PROBE_OLLAMA", "http://localhost:11434/v1"),
        ),
        (
            "lmstudio",
            env_or("LLMSH_PROBE_LMSTUDIO", "http://localhost:1234/v1"),
        ),
    ];
    let hs: Vec<_> = urls
        .into_iter()
        .map(|(id, url)| {
            std::thread::spawn(move || {
                llmsh_llm::list_models(&url, &None, PROBE_TIMEOUT)
                    .ok()
                    .map(|models| Found { id, url, models })
            })
        })
        .collect();
    let found: Vec<Found> = hs
        .into_iter()
        .filter_map(|h| h.join().ok().flatten())
        .collect();
    let key = std::env::var("OPENAI_API_KEY").is_ok_and(|k| !k.is_empty());
    let cloud = env_or("LLMSH_OPENAI_URL", OPENAI_URL);
    let Some((yaml, chat, comp)) = build(&found, key, &cloud) else {
        return Ok(());
    };
    let path = llm.path().expect("checked");
    if path.exists() {
        return Ok(());
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, yaml)?;
    std::fs::rename(&tmp, path)?;
    llm.set_notice(format!(
        "llmsh · chat: {chat} · complete: {comp} · llmsh config — изменить"
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
        let (y, chat, comp) = build(&found, false, "u").unwrap();
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
    fn key_makes_cloud_chat() {
        let found = [f("lmstudio", &["small-3b"])];
        let (y, chat, comp) = build(&found, true, "https://x/v1").unwrap();
        assert_eq!(chat, format!("cloud/{DEFAULT_CLOUD_MODEL}"));
        assert_eq!(comp, "lmstudio/small-3b");
        let c = Config::parse(&y).unwrap();
        assert_eq!(c.resolve("chat").unwrap().provider.id, "cloud");
        assert!(build(&[], false, "u").is_none());
        let (y, _, comp) = build(&[], true, "u").unwrap();
        assert_eq!(comp, "—");
        Config::parse(&y).unwrap();
    }
}
