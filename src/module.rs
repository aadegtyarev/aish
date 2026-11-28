//! App-level modules. A module is a feature of aish (history, completion,
//! highlighting, model access, later job notifications) that hooks into the
//! lifecycle below and may contribute plugins to the line editor. See
//! docs/PLUGINS.md.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
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
    /// Output is dropped (the screen it was meant for is gone).
    pub muted: AtomicBool,
    /// Held around "check muted + write" so `mute` is a barrier.
    write_lock: Mutex<()>,
    wake: Arc<File>,
    /// A status row (spinner) is the last row on screen: the next output
    /// replaces it. Changed only under `write_lock`.
    status_row: AtomicBool,
    /// Output has replaced the status row: no more spinner until `status_reset`.
    status_off: AtomicBool,
    /// The worker waits for one decision key (`Task::wait_key`): the
    /// controller hands keys to `offer_key` instead of buffering them.
    pub keys_wanted: AtomicBool,
    keys: Mutex<KeyState>,
}

/// Ignore decision keys this long after the prompt is drawn.
const KEY_DELAY: std::time::Duration = std::time::Duration::from_millis(300);
/// A decision key needs this much silence before it.
const KEY_QUIET: std::time::Duration = std::time::Duration::from_millis(700);

#[derive(Default)]
struct KeyState {
    key: Option<u8>,
    bad: bool,
    armed: Option<std::time::Instant>,
    last: Option<std::time::Instant>,
}

/// What `Task::wait_key` saw.
pub enum Key {
    /// `\r`, `s`, `a` or ESC (0x1b).
    Press(u8),
    /// Other input: show the hint and wait again.
    Other,
    /// Ctrl-C.
    Cancelled,
}

impl TaskShared {
    /// Input while the worker waits for a decision. It counts only when the
    /// chunk is exactly one key (Enter, `s`, `a`, lone Esc), the prompt has
    /// been up for `KEY_DELAY` and nothing else was typed in the last
    /// `KEY_QUIET` (so typing "yes" or "nah" never decides). Anything else
    /// asks the worker to show the hint.
    pub fn offer_input(&self, data: &[u8]) {
        let now = std::time::Instant::now();
        let Ok(mut st) = self.keys.lock() else { return };
        let quiet = st.last.is_none_or(|t| now.duration_since(t) >= KEY_QUIET);
        st.last = Some(now);
        if st.armed.is_none_or(|t| now < t) {
            return;
        }
        let key = match data {
            [b'\r'] | [b'\n'] => Some(b'\r'),
            [k @ (b's' | b'a' | 0x1b)] => Some(*k),
            _ => None,
        };
        match key {
            Some(k) if quiet => {
                st.key.get_or_insert(k);
            }
            _ => st.bad = true,
        }
    }
    /// Keys typed before the decision prompt appeared (they go to the
    /// typeahead) count towards the quiet time too.
    pub fn note_input(&self) {
        if let Ok(mut st) = self.keys.lock() {
            st.last = Some(std::time::Instant::now());
        }
    }

    /// Drop all further output; returns once no write is in flight.
    pub fn mute(&self) {
        let _g = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.muted.store(true, std::sync::atomic::Ordering::Relaxed);
    }

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
        let _g = self
            .shared
            .write_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.shared.muted.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let mut bytes = bytes;
        if self.shared.status_row.swap(false, Relaxed) {
            // the output takes the place of the status row
            let _ = (&*self.out).write_all(b"\r\x1b[2K");
            bytes = bytes.strip_prefix(b"\r\n").unwrap_or(bytes);
        }
        if !bytes.is_empty() {
            self.shared.status_off.store(true, Relaxed);
        }
        let _ = (&*self.out).write_all(bytes);
    }

    /// Block until the user decides (see `offer_input`).
    pub fn wait_key(&self) -> Key {
        if let Ok(mut g) = self.shared.keys.lock() {
            g.key = None;
            g.bad = false;
            g.armed = Some(std::time::Instant::now() + KEY_DELAY);
        }
        self.shared.keys_wanted.store(true, Relaxed);
        let r = loop {
            if self.cancelled() {
                break Key::Cancelled;
            }
            if let Ok(mut g) = self.shared.keys.lock() {
                if let Some(k) = g.key.take() {
                    break Key::Press(k);
                }
                if std::mem::take(&mut g.bad) {
                    break Key::Other;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        };
        self.shared.keys_wanted.store(false, Relaxed);
        r
    }

    /// A writer for the status row that a worker thread can own.
    pub fn status_writer(&self) -> StatusWriter {
        StatusWriter {
            out: self.out.clone(),
            shared: self.shared.clone(),
        }
    }

    /// Done: show the prompt again, with `prefill` typed into the line.
    pub fn finish(self, prefill: Option<String>) {
        self.complete(prefill);
    }

    fn complete(&self, prefill: Option<String>) {
        {
            let _g = self
                .shared
                .write_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if self.shared.status_row.swap(false, Relaxed) {
                let _ = (&*self.out).write_all(b"\r\x1b[2K");
            }
        }
        self.shared
            .finished
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut d) = self.shared.done.lock() {
            *d = Some(prefill);
        }
        let _ = (&*self.shared.wake).write(&[1]);
    }
}

/// Draws the one in-place status row of a task (see `editor::wait`).
#[derive(Clone)]
pub struct StatusWriter {
    out: Arc<File>,
    shared: Arc<TaskShared>,
}

impl StatusWriter {
    /// Forget that output replaced the status row: a new wait begins.
    pub fn reset(&self) {
        let _g = self
            .shared
            .write_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shared.status_off.store(false, Relaxed);
    }

    /// Show `row` (a complete row: bar and text) as the status row, in place
    /// when it is already there. False when output has taken over.
    pub fn show(&self, row: &[u8]) -> bool {
        let _g = self
            .shared
            .write_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.shared.muted.load(Relaxed) || self.shared.status_off.load(Relaxed) {
            return false;
        }
        let first = !self.shared.status_row.swap(true, Relaxed);
        let mut b = Vec::with_capacity(row.len() + 8);
        b.extend_from_slice(if first { b"\r\n" } else { b"\r\x1b[2K" });
        b.extend_from_slice(row);
        let _ = (&*self.out).write_all(&b);
        true
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
                muted: AtomicBool::new(false),
                write_lock: Mutex::new(()),
                wake: self.wake.clone(),
                status_row: AtomicBool::new(false),
                status_off: AtomicBool::new(false),
                keys_wanted: AtomicBool::new(false),
                keys: Mutex::new(KeyState::default()),
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

    #[test]
    fn typing_before_the_prompt_keeps_enter_from_deciding() {
        let past = std::time::Instant::now() - std::time::Duration::from_secs(1);
        for typed in [false, true] {
            let t = maker().make(&Theme::plain(), 80);
            let shared = t.shared.clone();
            shared.keys.lock().unwrap().armed = Some(past);
            if typed {
                shared.note_input();
            }
            shared.offer_input(b"\r");
            let st = shared.keys.lock().unwrap();
            assert_eq!(st.key, (!typed).then_some(b'\r'));
            assert_eq!(st.bad, typed);
        }
    }
}
