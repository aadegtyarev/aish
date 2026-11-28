//! Syntax highlighting with command resolution (PATH, builtins, aliases...).
//! The shell reports its PATH whenever it changes (private OSC), so
//! `export PATH=...` and venv activation are followed.

use std::sync::{Arc, Mutex};

use lineedit_wip::plugins::SyntaxHighlight;
use lineedit_wip::Plugin;

use crate::blocks::BlockTracker;
use crate::editor::resolve::{scan_path, Resolver};
use crate::module::{Env, Module};

#[derive(Default)]
pub struct HighlightModule {
    resolver: Option<Resolver>,
    tracker: Option<Arc<Mutex<BlockTracker>>>,
    seen_path: u64,
}

impl Module for HighlightModule {
    fn name(&self) -> &'static str {
        "highlight"
    }

    fn editor_plugins(&mut self, env: &Env) -> Vec<Box<dyn Plugin>> {
        let r = env.resolver.clone();
        self.resolver = Some(r.clone());
        self.tracker = Some(env.tracker.clone());
        let scan = r.clone();
        std::thread::spawn(move || {
            let (p, c) = scan_path(None);
            scan.set_path(p, c, false);
        });
        // our own command (`ai`, `aish`) is known although no such program exists
        let llm = env.llm.clone();
        vec![Box::new(SyntaxHighlight::new(move |w| {
            w == "aish" || llm.command().as_deref() == Some(w) || r.resolves(w)
        }))]
    }

    fn on_prompt(&mut self) {
        let (Some(r), Some(t)) = (&self.resolver, &self.tracker) else {
            return;
        };
        // The shell's PATH changed: rescan it off the key path.
        let new = t.lock().ok().and_then(|t| {
            let (p, v) = t.path();
            (v != self.seen_path).then(|| (p.map(str::to_owned), v))
        });
        if let Some((Some(path), v)) = new {
            self.seen_path = v;
            let r = r.clone();
            std::thread::spawn(move || {
                let (p, c) = scan_path(Some(path.into()));
                r.set_path(p, c, true);
            });
        } else {
            r.refresh_if_stale();
        }
    }
}
