//! App-level modules. A module is a feature of aish (history, completion,
//! highlighting, model access, later job notifications) that hooks into the
//! lifecycle below and may contribute plugins to the line editor. See
//! docs/PLUGINS.md.

use std::sync::{Arc, Mutex};

use aish_editor::plugins::SharedHistory;
use aish_editor::Plugin;

use crate::blocks::{Block, BlockTracker};
use crate::editor::resolve::Resolver;
use crate::task::Task;

/// Shared services handed to modules.
pub struct Env {
    pub llm: Arc<crate::llm::Llm>,
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

    /// The prompt is about to be shown: a module may continue earlier work
    /// (the agent resuming after the user ran its command) by taking `task`.
    /// Must be cheap and return false when there is nothing to do.
    fn on_resume(&mut self, _task: &mut Option<Task>) -> bool {
        false
    }

    /// A line the editor's intercept plugins claimed. Write what should be
    /// shown below it into `out` (no trailing newline) and return true if this
    /// module handled it; the line is then not sent to the shell. For a slow
    /// reply take `task` and answer from a worker thread instead.
    fn on_intercept(&mut self, _line: &str, _out: &mut Vec<u8>, _task: &mut Option<Task>) -> bool {
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
        Box::new(model::ModelModule::default()),
    ];
    all.into_iter()
        .filter(|m| enabled.is_none_or(|e| e.iter().any(|n| n == m.name())))
        .collect()
}
