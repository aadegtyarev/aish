//! Tab completion through the bash sidecar (see `complete.rs`).

use std::sync::{Arc, Mutex};

use aish_editor::completion::{CompletionSource, Outcome, Reply, Request};
use aish_editor::plugins::CompletionPager;
use aish_editor::Plugin;

use crate::blocks::BlockTracker;
use crate::complete::{complete_files, finish, Sidecar, TIMEOUT};
use crate::llm::cmd::SUBS;
use crate::llm::Llm;
use crate::module::{Env, Module};

pub struct CompletionModule;

type Slot = Arc<Mutex<Option<Arc<Sidecar>>>>;

/// Asks the sidecar on a worker thread so keystrokes are never blocked; the
/// answer arrives through the [`Reply`].
struct Bash {
    sidecar: Slot,
    tracker: Arc<Mutex<BlockTracker>>,
}

impl CompletionSource for Bash {
    fn complete(&mut self, req: &Request, reply: Reply) -> Outcome {
        let sidecar = self.sidecar.lock().ok().and_then(|g| g.clone());
        let tracker = self.tracker.clone();
        let words = req.words.clone();
        let req = Request {
            words,
            line: req.line.clone(),
            word_start: req.word_start,
        };
        std::thread::spawn(move || {
            let cwd = tracker
                .lock()
                .map(|t| t.cwd().to_owned())
                .unwrap_or_default();
            let cwd = if cwd.is_empty() { ".".to_owned() } else { cwd };
            let mut cands = sidecar
                .and_then(|s| s.complete(&req, &cwd, TIMEOUT))
                .unwrap_or_else(|| complete_files(req.cur(), &cwd));
            finish(&mut cands, &cwd);
            reply.send(cands);
        });
        Outcome::Pending
    }
}

/// Completes `aish` subcommands and model references; everything else goes
/// to the bash source.
struct WithLlm {
    inner: Bash,
    llm: Arc<Llm>,
}

impl CompletionSource for WithLlm {
    fn complete(&mut self, req: &Request, reply: Reply) -> Outcome {
        let ours = match req.words.first().map(String::as_str) {
            Some("aish") => true,
            Some(w) => self.llm.command().as_deref() == Some(w),
            None => false,
        };
        if !ours || req.words.len() < 2 {
            return self.inner.complete(req, reply);
        }
        let cur = req.cur();
        let items: Vec<String> = match req.words.len() {
            2 => SUBS.iter().map(|s| (*s).to_owned()).collect(),
            3 if matches!(req.words[1].as_str(), "chat" | "complete") => self
                .llm
                .config()
                .map(|c| {
                    c.providers
                        .iter()
                        .flat_map(|p| p.models.keys().map(move |m| format!("{}/{m}", p.id)))
                        .collect()
                })
                .unwrap_or_default(),
            _ => return Outcome::None,
        };
        let mut items: Vec<String> = items.into_iter().filter(|i| i.starts_with(cur)).collect();
        items.sort();
        if items.is_empty() {
            return Outcome::None;
        }
        Outcome::Ready(aish_editor::completion::Candidates {
            filenames: false,
            nospace: false,
            items,
        })
    }
}

impl Module for CompletionModule {
    fn name(&self) -> &'static str {
        "completion"
    }

    fn editor_plugins(&mut self, env: &Env) -> Vec<Box<dyn Plugin>> {
        let slot: Slot = Arc::new(Mutex::new(None));
        // The sidecar boots slowly (bash-completion, ~/.bashrc): start it on a
        // thread, then teach the highlighter its aliases/functions and PATH.
        let s2 = slot.clone();
        let resolver = env.resolver.clone();
        std::thread::spawn(move || {
            let Some(sc) = Sidecar::new() else { return };
            if let Ok(mut g) = s2.lock() {
                *g = Some(sc.clone());
            }
            if let Some(r) = sc.names(std::time::Duration::from_secs(10)) {
                // First item is the sidecar's PATH (unused: the shell reports
                // its live PATH itself); the rest are aliases/functions/builtins.
                resolver.set_extra(r.items.into_iter().skip(1));
            }
        });
        vec![Box::new(CompletionPager::new(WithLlm {
            inner: Bash {
                sidecar: slot,
                tracker: env.tracker.clone(),
            },
            llm: env.llm.clone(),
        }))]
    }
}
