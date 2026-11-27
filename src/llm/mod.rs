//! LLM features: shared state, `?` questions, the `aish` command, first run,
//! and model suggestions. Nothing here runs on the keystroke path: all network
//! work happens on worker threads.

pub mod agent;
pub mod allow;
pub mod ask;
pub mod cmd;
pub mod cmdsuggest;
pub mod context;
pub mod firstrun;
pub mod jobs;
pub mod mask;
pub mod render;
pub mod status;
pub mod suggest;
pub mod web;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use aish_llm::{Endpoint, Kind, Message};

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
    /// Offer function tools (the agent loop); false = single-step `?`.
    pub tools: bool,
    /// The provider has server-side web search (`web: native`, or OpenAI Responses).
    pub native_search: bool,
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
    /// Session-only role overrides (`aish chat cloud/main`).
    over: Mutex<HashMap<String, String>>,
    pub suggest_on: AtomicBool,
    pub convo: Mutex<Vec<Message>>,
    /// Held for a whole turn: a quick re-ask waits for the cancelled one.
    pub turn: Mutex<()>,
    notice: Mutex<Option<String>>,
    /// Runs once the pending notice was actually shown (update-check marks
    /// "notified" only then).
    notice_shown: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Wakes the controller (its wake pipe) when a notice arrives late.
    waker: std::sync::OnceLock<Arc<std::fs::File>>,
    /// Models that answered 400 to tools in this session (`provider/alias`).
    no_tools: Mutex<std::collections::HashSet<String>>,
    /// An agent turn waiting for the user to run a proposed command.
    pub pending: Mutex<Option<agent::Pending>>,
    pub jobs: Mutex<jobs::Jobs>,
    pub status: std::sync::OnceLock<status::Status>,
    /// The job poller found a dead pid: the controller should poke the shell.
    pub job_poll: AtomicBool,
    job_notices: Mutex<Vec<jobs::Finished>>,
}

impl Llm {
    pub fn new() -> Arc<Llm> {
        Arc::new(Llm {
            path: config::config_path(),
            cached: Mutex::new(Cached::default()),
            over: Mutex::new(HashMap::new()),
            suggest_on: AtomicBool::new(true),
            convo: Mutex::new(Vec::new()),
            turn: Mutex::new(()),
            notice: Mutex::new(None),
            notice_shown: Mutex::new(None),
            waker: std::sync::OnceLock::new(),
            no_tools: Mutex::default(),
            pending: Mutex::new(None),
            jobs: Mutex::default(),
            status: std::sync::OnceLock::new(),
            job_poll: AtomicBool::new(false),
            job_notices: Mutex::default(),
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

    /// Name of the in-session control command: `ai` unless configured;
    /// `None` when switched off (`command: off`). Always wins over any
    /// program of that name; `aish` itself works regardless.
    pub fn command(&self) -> Option<String> {
        match self.config().map(|c| c.command.clone()) {
            Some(Setting::Off) => None,
            Some(Setting::Value(v)) if !v.trim().is_empty() => Some(v.trim().to_owned()),
            _ => Some("ai".to_owned()),
        }
    }

    /// The line starts with one of our command names (`aish`, the configured
    /// one) and a space: arguments are ours, not for the model to complete.
    pub fn is_own_line(&self, t: &str) -> bool {
        let first = t.split(' ').next().unwrap_or("");
        t.contains(' ') && (first == "aish" || self.command().as_deref() == Some(first))
    }

    /// What to call the command in messages.
    pub fn command_label(&self) -> String {
        self.command().unwrap_or_else(|| "aish".to_owned())
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
            native_search: match &r.model.web {
                Setting::Value(_) => true,
                Setting::Off => false,
                Setting::Unset => {
                    r.provider.kind == ProviderType::Responses
                        && r.provider.url.contains("api.openai.com")
                }
            },
            tools: !matches!(r.model.tools, Setting::Off)
                && !self
                    .no_tools
                    .lock()
                    .is_ok_and(|s| s.contains(&format!("{}/{}", r.provider.id, r.alias))),
        })
    }

    pub fn mark_no_tools(&self, label: &str) {
        if let Ok(mut s) = self.no_tools.lock() {
            s.insert(label.to_owned());
        }
    }

    pub fn allow(&self) -> Vec<String> {
        self.config().map(|c| c.allow.clone()).unwrap_or_default()
    }

    pub fn set_notice(&self, s: String) {
        // model ids come from a server: never let them carry escapes
        if let Ok(mut n) = self.notice.lock() {
            *n = Some(render::sanitize(&s));
        }
        // the producer may finish after the prompt is up: have it shown now
        if let Some(w) = self.waker.get() {
            let _ = (&**w).write(&[1]);
        }
    }

    /// `set_notice`, and `shown` runs when the line has really been printed.
    pub fn set_notice_then(&self, s: String, shown: impl FnOnce() + Send + 'static) {
        if let Ok(mut f) = self.notice_shown.lock() {
            *f = Some(Box::new(shown));
        }
        self.set_notice(s);
    }

    pub fn set_waker(&self, w: Arc<std::fs::File>) {
        let _ = self.waker.set(w);
    }

    pub fn has_notice(&self) -> bool {
        self.notice.lock().is_ok_and(|n| n.is_some())
    }

    /// The notice taken with `take_notice` is on the screen.
    pub fn notice_shown(&self) {
        let f = self.notice_shown.lock().ok().and_then(|mut f| f.take());
        if let Some(f) = f {
            f();
        }
    }

    pub fn add_job_notices(&self, f: Vec<jobs::Finished>) {
        if let Ok(mut n) = self.job_notices.lock() {
            n.extend(f);
        }
    }

    pub fn take_job_notices(&self) -> Vec<jobs::Finished> {
        self.job_notices
            .lock()
            .map(|mut n| std::mem::take(&mut *n))
            .unwrap_or_default()
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
