//! History from $HISTFILE: ghost suggestions, Up/Down, Ctrl-R.

use lineedit_wip::plugins::{HistorySearch, HistorySuggest};
use lineedit_wip::{MemHistory, Plugin};

use crate::module::{Env, Module};

pub struct HistoryModule;

fn histfile() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("HISTFILE").filter(|v| !v.is_empty()) {
        return Some(p.into());
    }
    Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join(".bash_history"))
}

impl Module for HistoryModule {
    fn name(&self) -> &'static str {
        "history"
    }

    fn editor_plugins(&mut self, env: &Env) -> Vec<Box<dyn Plugin>> {
        let hist = env.history.clone();
        std::thread::spawn(move || {
            let data = histfile()
                .and_then(|p| std::fs::read(p).ok())
                .unwrap_or_default();
            hist.set(Box::new(MemHistory::parse(&String::from_utf8_lossy(&data))));
        });
        vec![
            Box::new(HistorySuggest::new(env.history.clone())),
            Box::new(HistorySearch::new(env.history.clone())),
        ]
    }
}
