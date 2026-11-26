//! App-level modules. A module is a feature of llmsh (history, completion,
//! highlighting, model access, later job notifications) that hooks into the
//! lifecycle below and may contribute plugins to the line editor. See
//! docs/PLUGINS.md.

use std::sync::{Arc, Mutex};

use lineedit_wip::plugins::SharedHistory;
use lineedit_wip::Plugin;

use crate::blocks::{Block, BlockTracker};
use crate::editor::resolve::Resolver;

/// Shared services handed to modules.
pub struct Env {
    pub tracker: Arc<Mutex<BlockTracker>>,
    pub resolver: Resolver,
    pub history: SharedHistory,
}

pub trait Module: Send {
    /// Name used in the config `modules:` list.
    fn name(&self) -> &'static str;

    /// Editor plugins to register (called once at startup). Anything slow
    /// (loading files, starting helper processes) must happen on a thread.
    fn editor_plugins(&mut self, _env: &Env) -> Vec<Box<dyn Plugin>> {
        Vec::new()
    }

    /// A command finished (command, output, exit code, ...).
    fn on_block_finished(&mut self, _block: &Block) {}

    /// The shell prompt is shown and the editor took over the input line.
    fn on_prompt(&mut self) {}

    /// A line the editor's intercept plugins claimed. Write what should be
    /// shown below it into `out` (no trailing newline) and return true if this
    /// module handled it; the line is then not sent to the shell.
    fn on_intercept(&mut self, _line: &str, _out: &mut Vec<u8>) -> bool {
        false
    }
}

/// All built-in modules, filtered by the config list (`None` = all).
pub fn builtin(enabled: Option<&[String]>) -> Vec<Box<dyn Module>> {
    use crate::modules::*;
    let all: Vec<Box<dyn Module>> = vec![
        Box::new(history::HistoryModule),
        Box::new(highlight::HighlightModule::default()),
        Box::new(completion::CompletionModule),
        Box::new(model::ModelModule),
    ];
    all.into_iter()
        .filter(|m| enabled.is_none_or(|e| e.iter().any(|n| n == m.name())))
        .collect()
}
