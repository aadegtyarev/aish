//! LLM features: shared state, `?` questions, the `llmsh` command, first run,
//! and model suggestions. Nothing here runs on the keystroke path: all network
//! work happens on worker threads.

pub mod ask;
pub mod cmd;
pub mod context;
pub mod firstrun;
pub mod mask;
pub mod render;
pub mod suggest;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use llmsh_llm::{Endpoint, Kind, Message};

use crate::config::{self, Config, ProviderType, Setting};

/// A role resolved to something we can call.
#[derive(Debug, Clone)]
pub struct Target {
    pub ep: Endpoint,
    pub provider: String,
    /// `provider/alias`
    pub label: String,
    pub window: Option<u64>,
    pub local: bool,
}

#[derive(Default)]
struct Cached {
    cfg: Option<Arc<Config>>,
    mtime: Option<SystemTime>,
    loaded: bool,
}

pub struct Llm {
    path: Option<PathBuf>,
    cached: Mutex<Cached>,
    /// Session-only role overrides (`llmsh chat cloud/main`).
    over: Mutex<HashMap<String, String>>,
    pub suggest_on: AtomicBool,
    pub convo: Mutex<Vec<Message>>,
    notice: Mutex<Option<String>>,
}

impl Llm {
    pub fn new() -> Arc<Llm> {
        Arc::new(Llm {
            path: config::config_path(),
            cached: Mutex::new(Cached::default()),
            over: Mutex::new(HashMap::new()),
            suggest_on: AtomicBool::new(true),
            convo: Mutex::new(Vec::new()),
            notice: Mutex::new(None),
        })
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    pub fn config_exists(&self) -> bool {
        self.path.as_ref().is_some_and(|p| p.exists())
    }

    /// The config, reloaded if the file's mtime changed since the last call.
    pub fn config(&self) -> Option<Arc<Config>> {
        let mtime = self
            .path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        let mut c = self.cached.lock().ok()?;
        if !c.loaded || c.mtime != mtime {
            c.loaded = true;
            c.mtime = mtime;
            match config::load() {
                Ok(v) => c.cfg = v.map(Arc::new),
                Err(e) => tracing::warn!("config reload: {e:#}"), // keep the old one
            }
        }
        c.cfg.clone()
    }

    pub fn set_override(&self, role: &str, id: &str) {
        if let Ok(mut o) = self.over.lock() {
            o.insert(role.to_owned(), id.to_owned());
        }
    }

    pub fn target(&self, role: &str) -> Option<Target> {
        let cfg = self.config()?;
        let over = self.over.lock().ok()?.get(role).cloned();
        let r = match over {
            Some(id) => cfg.resolve_ref(&id, &Setting::Unset).ok()?,
            None => cfg.resolve(role).ok()?,
        };
        Some(Target {
            ep: Endpoint {
                kind: match r.provider.kind {
                    ProviderType::Chat => Kind::Chat,
                    ProviderType::Responses => Kind::Responses,
                },
                url: r.provider.url.clone(),
                key: r.provider.api_key(),
                model: r.model.id.clone(),
                effort: r.effort.clone(),
            },
            provider: r.provider.id.clone(),
            label: format!("{}/{}", r.provider.id, r.alias),
            window: r.model.context_window.clone().value(),
            local: is_local(&r.provider.url),
        })
    }

    pub fn set_notice(&self, s: String) {
        // model ids come from a server: never let them carry escapes
        if let Ok(mut n) = self.notice.lock() {
            *n = Some(render::sanitize(&s));
        }
    }

    pub fn take_notice(&self) -> Option<String> {
        self.notice.lock().ok()?.take()
    }
}

/// localhost / loopback / private LAN host in a URL.
pub fn is_local(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |x| x.1);
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    let auth = auth.rsplit_once('@').map_or(auth, |x| x.1);
    let host = if let Some(h) = auth.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        auth.split(':').next().unwrap_or("")
    };
    let h = host.to_ascii_lowercase();
    let h = h.trim_end_matches('.');
    if h == "localhost" || h == "::1" || h.ends_with(".local") || h.ends_with(".localhost") {
        return true;
    }
    let o: Vec<u8> = h.split('.').filter_map(|p| p.parse().ok()).collect();
    if o.len() != 4 || h.split('.').count() != 4 {
        return h.contains(':')
            && (h.starts_with("fe80:") || h.starts_with("fc") || h.starts_with("fd"));
    }
    o[0] == 127
        || o[0] == 0
        || o[0] == 10
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 172 && (16..32).contains(&o[1]))
        || (o[0] == 169 && o[1] == 254)
}

#[cfg(test)]
mod tests {
    use super::is_local;

    #[test]
    fn locality() {
        for u in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:1/v1",
            "http://[::1]:8080/v1",
            "http://192.168.1.5/v1",
            "http://10.0.0.2:1234/v1",
            "http://172.20.0.1/v1",
            "http://box.local/v1",
            "http://0.0.0.0:8080/v1",
            "http://localhost./v1",
            "http://[fd12:3456::1]/v1",
            "http://localhost:1#@evil.com/",
            "http://localhost?x=@evil.com",
        ] {
            assert!(is_local(u), "{u}");
        }
        for u in [
            "https://api.openai.com/v1",
            "http://172.32.0.1/v1",
            "http://8.8.8.8/v1",
            "https://localhost.evil.com/v1",
            "https://evil.com#@localhost/v1",
            "https://localhost@evil.com/v1",
        ] {
            assert!(!is_local(u), "{u}");
        }
    }
}
