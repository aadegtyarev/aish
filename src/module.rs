//! App-level modules. A module is a feature of llmsh (history, completion,
//! highlighting, model access, later job notifications) that hooks into the
//! lifecycle below and may contribute plugins to the line editor. See
//! docs/PLUGINS.md.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use lineedit_wip::plugins::SharedHistory;
use lineedit_wip::{Plugin, Theme};

use crate::blocks::{Block, BlockTracker};
use crate::editor::resolve::Resolver;

/// Shared services handed to modules.
pub struct Env {
    pub llm: Arc<crate::llm::Llm>,
    pub tasks: TaskMaker,
    pub tracker: Arc<Mutex<BlockTracker>>,
    pub resolver: Resolver,
    pub history: SharedHistory,
}

/// State shared between a running [`Task`] and the controller.
pub struct TaskShared {
    pub cancel: AtomicBool,
    /// `Some(prefill)` once the task is finished.
    done: Mutex<Option<Option<String>>>,
    finished: AtomicBool,
    wake: Arc<File>,
}

impl TaskShared {
    pub fn take_done(&self) -> Option<Option<String>> {
        self.done.lock().ok()?.take()
    }
}

/// An asynchronous reply to an intercepted line (an answer streaming in on a
/// worker thread). The controller owns the terminal in the meantime: keys are
/// buffered (Ctrl-C sets `cancel`), and when the task finishes the prompt
/// comes back with `prefill` in the input line.
pub struct Task {
    pub out: Arc<File>,
    pub theme: Theme,
    pub cols: u16,
    pub shared: Arc<TaskShared>,
}

impl Task {
    pub fn cancelled(&self) -> bool {
        self.shared
            .cancel
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn write(&self, bytes: &[u8]) {
        let _ = (&*self.out).write_all(bytes);
    }

    /// Done: show the prompt again, with `prefill` typed into the line.
    pub fn finish(self, prefill: Option<String>) {
        self.complete(prefill);
    }

    fn complete(&self, prefill: Option<String>) {
        self.shared
            .finished
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut d) = self.shared.done.lock() {
            *d = Some(prefill);
        }
        let _ = (&*self.shared.wake).write(&[1]);
    }
}

/// A task dropped without `finish` (panic, early return) still hands the
/// prompt back, so the terminal never stays captured.
impl Drop for Task {
    fn drop(&mut self) {
        if !self
            .shared
            .finished
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.complete(None);
        }
    }
}

/// Creates [`Task`]s (terminal output + wake-up pipe).
#[derive(Clone)]
pub struct TaskMaker {
    pub stdout: Arc<File>,
    pub wake: Arc<File>,
}

impl TaskMaker {
    pub fn make(&self, theme: &Theme, cols: u16) -> Task {
        Task {
            out: self.stdout.clone(),
            theme: theme.clone(),
            cols,
            shared: Arc::new(TaskShared {
                cancel: AtomicBool::new(false),
                done: Mutex::new(None),
                finished: AtomicBool::new(false),
                wake: self.wake.clone(),
            }),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn maker() -> TaskMaker {
        let null = || Arc::new(File::options().write(true).open("/dev/null").unwrap());
        TaskMaker {
            stdout: null(),
            wake: null(),
        }
    }

    #[test]
    fn dropped_task_hands_the_prompt_back() {
        let t = maker().make(&Theme::plain(), 80);
        let shared = t.shared.clone();
        assert!(shared.take_done().is_none());
        drop(t);
        assert_eq!(shared.take_done(), Some(None));
    }

    #[test]
    fn finished_task_keeps_its_prefill() {
        let t = maker().make(&Theme::plain(), 80);
        let shared = t.shared.clone();
        t.finish(Some("ls".into()));
        assert_eq!(shared.take_done(), Some(Some("ls".into())));
        assert!(shared.take_done().is_none()); // drop did not finish a second time
    }
}
