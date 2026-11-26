//! Tab completion through the bash sidecar (see `complete.rs`).

use std::sync::{Arc, Mutex};

use lineedit_wip::completion::{CompletionSource, Outcome, Reply, Request};
use lineedit_wip::plugins::CompletionPager;
use lineedit_wip::Plugin;

use crate::blocks::BlockTracker;
use crate::complete::{complete_files, finish, Sidecar, TIMEOUT};
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
                let mut names = r.items.into_iter();
                let sc_path = names.next().map(std::ffi::OsString::from);
                resolver.set_extra(names);
                if let Some(p) = sc_path {
                    let (path, cmds) = crate::editor::resolve::scan_path(Some(p));
                    resolver.set_path(path, cmds, true);
                }
            }
        });
        vec![Box::new(CompletionPager::new(Bash {
            sidecar: slot,
            tracker: env.tracker.clone(),
        }))]
    }
}
