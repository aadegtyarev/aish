//! LLM features: shared state, `?` questions, the `aish` command, first run,
//! and model suggestions. Nothing here runs on the keystroke path: all network
//! work happens on worker threads.

pub mod agent;
pub mod allow;
pub mod ask;
pub mod cmd;
pub mod cmdsuggest;
pub mod compact;
pub mod context;
pub mod firstrun;
pub mod hard;
pub mod jobs;
pub mod mask;
pub mod opts;
pub mod prefix;
pub mod render;
pub mod status;
pub mod suggest;
pub mod web;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

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
    /// Models that did not answer lately (`provider/alias` -> when): tried last.
    failed: Mutex<HashMap<String, Instant>>,
    /// An agent turn waiting for the user to run a proposed command.
    pub pending: Mutex<Option<agent::Pending>>,
    pub jobs: Mutex<jobs::Jobs>,
    /// Session overrides of `run:` and `tools:`.
    pub opts: Mutex<opts::Opts>,
    pub status: std::sync::OnceLock<status::Status>,
    /// The job poller found a dead pid: the controller should poke the shell.
    pub job_poll: AtomicBool,
    job_notices: Mutex<Vec<jobs::Finished>>,
    /// The question prefix, read once per session.
    prefix: std::sync::OnceLock<char>,
    /// Bumped by every reset: a summary in the making must not overwrite it.
    pub convo_gen: std::sync::atomic::AtomicU64,
    /// The summary at the head of the memory was made from unmasked text.
    pub summary_raw: AtomicBool,
    /// Context windows asked from the server (`url model` -> `n_ctx`, if it said).
    windows: Mutex<HashMap<String, Option<u64>>>,
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
            failed: Mutex::default(),
            pending: Mutex::new(None),
            jobs: Mutex::default(),
            opts: Mutex::default(),
            status: std::sync::OnceLock::new(),
            job_poll: AtomicBool::new(false),
            job_notices: Mutex::default(),
            prefix: std::sync::OnceLock::new(),
            convo_gen: std::sync::atomic::AtomicU64::new(0),
            summary_raw: AtomicBool::new(false),
            windows: Mutex::default(),
        })
    }

    /// The character that starts a question (`prefix:` in the config, `?`
    /// by default). Read once: the editor's marker and highlighting follow it.
    pub fn prefix(&self) -> char {
        *self
            .prefix
            .get_or_init(|| self.config().map_or('?', |c| c.prefix()))
    }

    /// `?n`: forget the conversation. False (nothing changed) while a turn
    /// or a compaction is running.
    pub fn reset_memory(&self) -> bool {
        let Ok(_turn) = self.turn.try_lock() else {
            return false;
        };
        if let Ok(mut c) = self.convo.lock() {
            c.clear();
            self.convo_gen
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.summary_raw
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        true
    }

    /// The context window of the model in tokens: `context_window` from the
    /// config, else what a local llama.cpp server says (`/props`, asked once
    /// per model), else 8192.
    pub fn window(&self, t: &Target) -> u64 {
        if let Some(w) = t.window {
            return w;
        }
        if t.local {
            let key = format!("{} {}", t.ep.url, t.ep.model);
            let known = self.windows.lock().ok().and_then(|w| w.get(&key).copied());
            let n_ctx = match known {
                Some(n) => n,
                None => {
                    let n = compact::fetch_n_ctx(&t.ep.url, &t.ep.key);
                    if let Ok(mut w) = self.windows.lock() {
                        w.insert(key, n);
                    }
                    n
                }
            };
            if let Some(n) = n_ctx {
                return n;
            }
        }
        compact::DEFAULT_WINDOW
    }

    /// The window as far as it is known without asking the server.
    pub fn window_known(&self, t: &Target) -> u64 {
        let key = format!("{} {}", t.ep.url, t.ep.model);
        t.window
            .or_else(|| {
                self.windows
                    .lock()
                    .ok()
                    .and_then(|w| w.get(&key).copied().flatten())
            })
            .unwrap_or(compact::DEFAULT_WINDOW)
    }

    /// The model that writes summaries: `compact.model`, else `chat`.
    pub fn compact_target(&self, chat: &Target) -> Target {
        let cfg = self.config();
        cfg.as_ref()
            .and_then(|c| {
                let id = c.compact.as_ref()?.model.as_deref()?;
                c.resolve_ref(id, &Setting::Unset)
                    .ok()
                    .map(|r| self.make_target(&r))
            })
            .unwrap_or_else(|| chat.clone())
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
            let loaded = match &self.path {
                Some(p) => config::load_from(p),
                None => Ok(None),
            };
            match loaded {
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

    /// The first model of the role that is not known to be down.
    pub fn target(&self, role: &str) -> Option<Target> {
        self.targets(role).into_iter().next()
    }

    /// Every model of the role in fallback order; ones that failed in the last
    /// `FAIL_MEMORY` go last (still tried when nothing else is left).
    pub fn targets(&self, role: &str) -> Vec<Target> {
        self.targets_at(role, Instant::now())
    }

    pub fn targets_at(&self, role: &str, now: Instant) -> Vec<Target> {
        let Some(cfg) = self.config() else {
            return Vec::new();
        };
        let over = self.over.lock().ok().and_then(|o| o.get(role).cloned());
        let all: Vec<Target> = match over {
            Some(id) => cfg
                .resolve_ref(&id, &Setting::Unset)
                .ok()
                .map(|r| vec![self.make_target(&r)])
                .unwrap_or_default(),
            None => cfg
                .resolve_all(role)
                .map(|v| v.iter().map(|r| self.make_target(r)).collect())
                .unwrap_or_default(),
        };
        let (down, up): (Vec<_>, Vec<_>) = all
            .into_iter()
            .partition(|t| self.is_down(role, &t.label, now));
        up.into_iter().chain(down).collect()
    }

    fn make_target(&self, r: &config::Resolved<'_>) -> Target {
        Target {
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
        }
    }

    fn is_down(&self, role: &str, label: &str, now: Instant) -> bool {
        self.failed.lock().is_ok_and(|f| {
            f.get(&format!("{role}\0{label}"))
                .is_some_and(|t| now.saturating_duration_since(*t) < fail_memory())
        })
    }

    /// The model did not answer: following requests go to the next one.
    /// Remembered per role: a slow `complete` never hides a model from `chat`.
    pub fn mark_failed(&self, role: &str, label: &str) {
        self.mark_failed_at(role, label, Instant::now());
    }

    pub fn mark_failed_at(&self, role: &str, label: &str, now: Instant) {
        if let Ok(mut f) = self.failed.lock() {
            f.insert(format!("{role}\0{label}"), now);
        }
    }

    /// The role's models that are not marked down.
    pub fn targets_up(&self, role: &str) -> Vec<Target> {
        let now = Instant::now();
        self.targets_at(role, now)
            .into_iter()
            .filter(|t| !self.is_down(role, &t.label, now))
            .collect()
    }

    /// `provider/alias` of every model of the role, configured order (a
    /// session override replaces them).
    pub fn labels(&self, role: &str) -> Vec<String> {
        let Some(cfg) = self.config() else {
            return Vec::new();
        };
        let over = self.over.lock().ok().and_then(|o| o.get(role).cloned());
        match over {
            Some(id) => cfg
                .resolve_ref(&id, &Setting::Unset)
                .map(|r| vec![format!("{}/{}", r.provider.id, r.alias)])
                .unwrap_or_default(),
            None => cfg
                .resolve_all(role)
                .map(|v| {
                    v.iter()
                        .map(|r| format!("{}/{}", r.provider.id, r.alias))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    pub fn clear_override(&self, role: &str) {
        if let Ok(mut o) = self.over.lock() {
            o.remove(role);
        }
    }

    pub fn clear_overrides(&self) {
        if let Ok(mut o) = self.over.lock() {
            o.clear();
        }
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

/// How long a model that did not answer is skipped. `AISH_FAIL_MEMORY_MS`
/// overrides it (tests).
pub fn fail_memory() -> Duration {
    std::env::var("AISH_FAIL_MEMORY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_secs(60), Duration::from_millis)
}

/// Errors that also mark the model down (a mere timeout of a short
/// suggestion request does not).
pub fn is_down_error(e: &aish_llm::Error) -> bool {
    use aish_llm::Error;
    match e {
        Error::Connect(_) => true,
        Error::Http { status, .. } => *status == 404 || *status >= 500,
        _ => false,
    }
}

/// Errors that make the next model of a role worth trying (nothing was
/// answered yet): no connection, timeout, server error, model not found.
pub fn failover(e: &aish_llm::Error) -> bool {
    use aish_llm::Error;
    match e {
        Error::Connect(_) | Error::Timeout => true,
        Error::Http { status, .. } => *status == 404 || *status >= 500,
        _ => false,
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
    use super::*;

    fn llm_with(cfg: &str) -> (Arc<Llm>, tempfile::TempDir) {
        let d = tempfile::TempDir::new().unwrap();
        let p = d.path().join("config.yaml");
        std::fs::write(&p, cfg).unwrap();
        let l = Llm::new();
        let l = Arc::new(Llm {
            path: Some(p),
            ..Arc::try_unwrap(l).ok().unwrap()
        });
        (l, d)
    }

    const FB: &str = "providers:\n  - {id: p, url: 'http://127.0.0.1:1/v1', type: chat, models: {a: {id: A}, b: {id: B}, c: {id: C}}}\nroles:\n  chat: {id: [a, b, c]}\n  complete: b\n";

    fn labels(l: &Llm, now: Instant) -> Vec<String> {
        l.targets_at("chat", now)
            .into_iter()
            .map(|t| t.label)
            .collect()
    }

    #[test]
    fn fallback_order_and_sixty_second_memory() {
        let (l, _d) = llm_with(FB);
        let t0 = Instant::now();
        assert_eq!(labels(&l, t0), ["p/a", "p/b", "p/c"]);
        assert_eq!(l.target("complete").unwrap().label, "p/b");
        l.mark_failed_at("chat", "p/a", t0);
        // down for 60 s: tried last; afterwards first again
        assert_eq!(
            labels(&l, t0 + Duration::from_secs(59)),
            ["p/b", "p/c", "p/a"]
        );
        assert_eq!(
            labels(&l, t0 + Duration::from_secs(60)),
            ["p/a", "p/b", "p/c"]
        );
        l.mark_failed_at("chat", "p/b", t0);
        assert_eq!(
            labels(&l, t0 + Duration::from_secs(1)),
            ["p/c", "p/a", "p/b"]
        );
        // all down: configured order is kept
        l.mark_failed_at("chat", "p/c", t0);
        assert_eq!(labels(&l, t0), ["p/a", "p/b", "p/c"]);
    }

    #[test]
    fn failover_errors() {
        use aish_llm::Error;
        assert!(failover(&Error::Connect("x".into())));
        assert!(failover(&Error::Timeout));
        assert!(failover(&Error::Http {
            status: 503,
            message: String::new()
        }));
        assert!(failover(&Error::Http {
            status: 404,
            message: String::new()
        }));
        assert!(!failover(&Error::Http {
            status: 401,
            message: String::new()
        }));
        assert!(!failover(&Error::Http {
            status: 400,
            message: String::new()
        }));
        assert!(!failover(&Error::Api("x".into())));
        assert!(!failover(&Error::Cancelled));
    }

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
