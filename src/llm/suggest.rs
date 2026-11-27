//! Model ghost suggestions: an editor plugin that never blocks. Keystrokes
//! only send a small job to a worker thread, which waits for a pause
//! (debounce), asks the `complete` model and delivers the result through the
//! editor's generation-checked `Handle`.

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aish_llm::{Message, Request};
use lineedit_wip::plugins::SharedHistory;
use lineedit_wip::{Ctx, Handle, Handled, Key, Plugin};

use super::cmd::is_aish_line;
use super::mask::mask;
use super::{Llm, Target};
use crate::blocks::BlockTracker;
use crate::modules::model::is_model_line;

pub const DEBOUNCE: Duration = Duration::from_millis(150);
const CACHE: usize = 256;
const BACKOFF: Duration = Duration::from_secs(60);
const MAX_LEN: usize = 200;

const SYSTEM: &str = "You complete a bash command line. Reply with ONLY the text that continues \
the line exactly from its end: no explanation, no markdown, no quotes, do not repeat the line. \
One line. Reply with nothing if unsure.";

/// Names from `ls` of the working directory, refreshed when a command finished.
#[derive(Default)]
pub struct LsCache {
    state: Mutex<(String, Vec<String>)>,
    dirty: AtomicBool,
}

impl LsCache {
    pub fn invalidate(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn names(&self, cwd: &str) -> Vec<String> {
        let Ok(mut s) = self.state.lock() else {
            return Vec::new();
        };
        if s.0 != cwd || self.dirty.swap(false, Ordering::Relaxed) {
            let mut v: Vec<String> = std::fs::read_dir(if cwd.is_empty() { "." } else { cwd })
                .map(|d| {
                    d.flatten()
                        .map(|e| {
                            let n = e.file_name().to_string_lossy().into_owned();
                            if e.path().is_dir() {
                                format!("{n}/")
                            } else {
                                n
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v.truncate(30);
            *s = (cwd.to_owned(), v);
        }
        s.1.clone()
    }
}

struct Job {
    gen: u64,
    line: String,
    handle: Handle,
    immediate: bool,
    cancel: Arc<AtomicBool>,
}

/// Result sent to `on_delivery`: the whole suggested line (or empty).
struct Suggestion {
    line: String,
    full: String,
}

/// Normalize a raw model reply into the continuation to append, if usable.
pub fn clean(line: &str, reply: &str) -> Option<String> {
    let first = reply.split('\n').next().unwrap_or("");
    let mut t = first.trim_end_matches('\r');
    if let Some(rest) = t.strip_prefix(line) {
        t = rest; // the model repeated the line
    }
    if t.trim().is_empty() || t.chars().any(char::is_control) || line.len() + t.len() > MAX_LEN {
        return None;
    }
    Some(t.to_owned())
}

struct Lru {
    map: HashMap<String, String>,
    order: VecDeque<String>,
}

impl Lru {
    fn put(&mut self, k: String, v: String) {
        if self.map.insert(k.clone(), v).is_none() {
            self.order.push_back(k);
            if self.order.len() > CACHE {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
}

pub struct ModelSuggest {
    llm: Arc<Llm>,
    history: SharedHistory,
    tx: mpsc::Sender<Job>,
    cancel: Arc<AtomicBool>,
    cache: Lru,
    /// Full suggested line currently shown for the typed text.
    cur: Option<String>,
}

impl ModelSuggest {
    pub fn new(
        llm: Arc<Llm>,
        history: SharedHistory,
        tracker: Arc<Mutex<BlockTracker>>,
        ls: Arc<LsCache>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let l2 = llm.clone();
        std::thread::spawn(move || worker(&l2, &tracker, &ls, &rx));
        ModelSuggest {
            llm,
            history,
            tx,
            cancel: Arc::new(AtomicBool::new(false)),
            cache: Lru {
                map: HashMap::new(),
                order: VecDeque::new(),
            },
            cur: None,
        }
    }

    fn eligible(&self, ctx: &Ctx) -> bool {
        let t = ctx.text();
        !t.is_empty()
            && ctx.cursor() == t.len()
            && !is_model_line(t)
            && !is_aish_line(t)
            && !self.llm.is_own_line(t)
            && self.llm.suggest_on.load(Ordering::Relaxed)
    }

    fn request(&mut self, ctx: &mut Ctx, immediate: bool) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        let t = ctx.text().to_owned();
        if !self.eligible(ctx) {
            self.cur = None;
            return;
        }
        // Typed what was suggested: keep showing the remainder.
        if let Some(c) = &self.cur {
            if c.starts_with(&t) && c.len() > t.len() {
                return;
            }
        }
        self.cur = None;
        if self.history.suggest(&t).is_some() {
            return;
        }
        if let Some(full) = self.cache.map.get(&t) {
            if full.len() > t.len() {
                self.cur = Some(full.clone());
            }
            return;
        }
        let _ = self.tx.send(Job {
            gen: ctx.generation(),
            line: t,
            handle: ctx.handle(),
            immediate,
            cancel: self.cancel.clone(),
        });
    }
}

impl Plugin for ModelSuggest {
    fn name(&self) -> &'static str {
        "model-suggest"
    }
    /// Below history: used only when history has nothing.
    fn priority(&self) -> i32 {
        -10
    }
    fn bindings(&self) -> &'static [(Key, &'static str)] {
        &[(Key::Ctrl(b' '), "model-suggest")]
    }
    fn on_action(&mut self, ctx: &mut Ctx, action: &str) -> Handled {
        if action != "model-suggest" {
            return Handled::Pass;
        }
        // Forget a cached empty answer: the user asks again.
        self.cache.map.remove(ctx.text());
        self.request(ctx, true);
        Handled::Yes
    }
    fn on_buffer_change(&mut self, ctx: &mut Ctx) {
        self.request(ctx, false);
    }
    fn suggest(&mut self, ctx: &mut Ctx) -> Option<&str> {
        let t = ctx.text();
        self.cur
            .as_deref()
            .filter(|c| c.starts_with(t) && c.len() > t.len())
    }
    fn on_delivery(&mut self, ctx: &mut Ctx, payload: Box<dyn Any + Send>) {
        let Ok(s) = payload.downcast::<Suggestion>() else {
            return;
        };
        if s.line != ctx.text() {
            return;
        }
        self.cache.put(s.line.clone(), s.full.clone());
        self.cur = (s.full.len() > s.line.len()).then_some(s.full);
    }
}

fn worker(
    llm: &Arc<Llm>,
    tracker: &Arc<Mutex<BlockTracker>>,
    ls: &Arc<LsCache>,
    rx: &mpsc::Receiver<Job>,
) {
    let mut fails = 0u32;
    let mut quiet_until: Option<Instant> = None;
    while let Ok(mut job) = rx.recv() {
        while !job.immediate {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(j) => job = j,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if job.cancel.load(Ordering::Relaxed) {
            continue;
        }
        if quiet_until.is_some_and(|t| Instant::now() < t) && !job.immediate {
            continue;
        }
        let Some(t) = llm.target("complete") else {
            continue;
        };
        if let Some(s) = llm.status.get() {
            s.begin();
        }
        let res = ask(&t, tracker, ls, &job);
        if let Some(s) = llm.status.get() {
            s.end();
        }
        match res {
            Ok(full) => {
                fails = 0;
                job.handle.deliver(
                    job.gen,
                    Box::new(Suggestion {
                        line: job.line.clone(),
                        full: full.unwrap_or_default(),
                    }),
                );
            }
            Err(aish_llm::Error::Cancelled) => {}
            // Stale (cancelled) jobs say nothing about the server's health.
            Err(_) if job.cancel.load(Ordering::Relaxed) => {}
            Err(e) => {
                tracing::debug!("suggest: {e}");
                fails += 1;
                if fails >= 3 {
                    fails = 0;
                    quiet_until = Some(Instant::now() + BACKOFF);
                }
            }
        }
    }
}

fn ask(
    t: &Target,
    tracker: &Arc<Mutex<BlockTracker>>,
    ls: &LsCache,
    job: &Job,
) -> Result<Option<String>, aish_llm::Error> {
    let (cwd, recent) = match tracker.lock() {
        Ok(tr) => (
            tr.cwd().to_owned(),
            tr.blocks()
                .last(5)
                .map(|b| {
                    format!(
                        "$ {} (exit {})",
                        b.command.lines().next().unwrap_or(""),
                        b.exit_code.map_or("?".into(), |c| c.to_string())
                    )
                })
                .collect::<Vec<_>>(),
        ),
        Err(_) => (String::new(), Vec::new()),
    };
    let names = ls.names(&cwd);
    let mut ctx = format!(
        "cwd: {cwd}\nrecent commands:\n{}\nfiles: {}\nLine so far (continue it from its very end):\n",
        recent.join("\n"),
        names.join(" ")
    );
    if !t.local {
        ctx = mask(&ctx);
    }
    let line = if t.local {
        job.line.clone()
    } else {
        mask(&job.line)
    };
    ctx.push_str(&line);
    let req = Request {
        max_tokens: 48,
        temperature: Some(0.0),
        stop: vec!["\n".into()],
        first_token_timeout: aish_llm::FIRST_TOKEN_COMPLETE,
        ..Request::chat(SYSTEM, vec![Message::user(ctx)])
    };
    // Only the first line matters: stop (and close the connection) at "\n".
    let mut reply = String::new();
    let mut seen_nl = false;
    let r = aish_llm::stream(&t.ep, &req, &job.cancel, |e| {
        let aish_llm::Event::Delta(d) = e else { return };
        reply.push_str(&d);
        if reply.contains('\n') && !seen_nl {
            seen_nl = true;
            job.cancel.store(true, Ordering::Relaxed); // this job is done: close it
        }
    });
    match r {
        Err(aish_llm::Error::Cancelled) if seen_nl => {}
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    Ok(clean(&job.line, &reply).map(|c| format!("{}{c}", job.line)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning() {
        assert_eq!(
            clean("git che", "ckout main\nmore").as_deref(),
            Some("ckout main")
        );
        assert_eq!(
            clean("git che", "git checkout x").as_deref(),
            Some("ckout x")
        );
        assert_eq!(clean("ls", "   "), None);
        assert_eq!(clean("ls", "\x1b[31m"), None);
        assert_eq!(clean("ls", &"a".repeat(300)), None);
        assert_eq!(clean("ls", "ls"), None);
        assert_eq!(clean("ls ", "-la").as_deref(), Some("-la"));
    }

    #[test]
    fn lru_evicts_oldest() {
        let mut l = Lru {
            map: HashMap::new(),
            order: VecDeque::new(),
        };
        for i in 0..300 {
            l.put(format!("k{i}"), "v".into());
        }
        assert_eq!(l.map.len(), CACHE);
        assert!(!l.map.contains_key("k0") && l.map.contains_key("k299"));
    }
}
