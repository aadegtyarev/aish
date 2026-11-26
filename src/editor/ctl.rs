//! I/O side of prompt mode: decides when we own the input line, feeds bytes
//! to the editor, draws, and hands finished lines to bash.
//!
//! Lock discipline: one mutex around [`Controller`], taken by the input thread
//! (per read), the output thread (per chunk) and the resize thread. Bulk
//! passthrough writes happen outside the lock, and the controller never writes
//! to the PTY itself: bytes for bash are collected in an outbox that the caller
//! writes after dropping the lock (a full PTY must not stall the other thread).
//! Controller calls run under `catch_unwind` ([`with`]): a panic in the editor
//! or a plugin disables prompt mode for the rest of the session.

use std::fs::File;
use std::io::Write;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lineedit_wip::plugins::SharedHistory;
use lineedit_wip::{CursorReportFilter, Editor, Event, MemHistory, Theme};

use super::resolve::Resolver;
use super::scan::Scan;
use crate::blocks::{Block, BlockTracker};
use crate::llm::Llm;
use crate::module::{self, Env, Module, TaskMaker, TaskShared};

/// Output silence after OSC 133;B before we take over the input line.
pub const QUIET: Duration = Duration::from_millis(10);
/// A lone ESC is an Escape key if nothing follows within this time.
pub const ESC_WAIT: Duration = Duration::from_millis(30);
const DSR_TTL: Duration = Duration::from_secs(1);

/// Bound to `redraw-current-line` in rc.bash.
const REDRAW_KEY: &[u8] = b"\x1b[1337~";

pub type Shared = Arc<Mutex<Controller>>;

pub fn lock(s: &Shared) -> MutexGuard<'_, Controller> {
    s.lock().unwrap_or_else(|e| {
        tracing::error!("controller mutex poisoned: prompt mode disabled");
        let mut g = e.into_inner();
        g.disabled = true;
        g
    })
}

/// Run `f` on the controller under the lock, catching panics. Returns its
/// result and the bytes to write to bash (write them after this returns, i.e.
/// without the lock). On panic: log, disable prompt mode, return `default`.
pub fn with<R>(c: &Shared, default: R, f: impl FnOnce(&mut Controller) -> R) -> (R, Vec<u8>) {
    let mut g = lock(c);
    crate::term::IN_GUARD.with(|x| x.set(true));
    let r = catch_unwind(AssertUnwindSafe(|| f(&mut g)));
    crate::term::IN_GUARD.with(|x| x.set(false));
    let r = match r {
        Ok(r) => r,
        Err(e) => {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            tracing::error!("line editor panicked ({msg}); falling back to plain bash input");
            let _ = catch_unwind(AssertUnwindSafe(|| g.disable()));
            default
        }
    };
    (r, g.take_outbox())
}

pub enum Input {
    /// Send the first N bytes of the buffer to bash untouched.
    Forward(usize),
    /// Send these bytes (the input with cursor reports removed).
    ForwardOwned(Vec<u8>),
    /// Consumed; the flag says a partial key is pending (wait `ESC_WAIT`).
    Handled(bool),
}

pub struct Controller {
    ed: Editor,
    scan: Scan,
    prompt: bool,
    /// Bytes went to bash while its prompt was waiting: its line is not empty.
    dirty: bool,
    dsr: Option<Instant>,
    dsr_filter: CursorReportFilter,
    use_dsr: bool,
    disabled: bool,
    outbox: Vec<u8>,
    stdout: File,
    modules: Vec<Box<dyn Module>>,
    events: Vec<Event>,
    notice: Vec<u8>,
    seen_blocks: u64,
    llm: Arc<Llm>,
    tasks: TaskMaker,
    /// An answer is streaming: keys are buffered, Ctrl-C cancels.
    task: Option<Arc<TaskShared>>,
    /// Text for the input line at the next prompt (a command from the model).
    pending_prefill: Option<String>,
    typeahead: Vec<u8>,
}

fn width() -> u16 {
    crate::term::window_size().ws_col.max(1)
}

/// Whether prompt mode should be used at all for this environment.
pub fn wanted() -> bool {
    std::env::var_os("LLMSH_EDITOR").is_none_or(|v| v != "0")
        && std::env::var("TERM").is_ok_and(|t| !t.is_empty() && t != "dumb")
}

impl Controller {
    /// Build the controller and its editor from the enabled modules.
    pub fn start(
        stdout: File,
        tracker: Arc<Mutex<BlockTracker>>,
        enabled: Option<&[String]>,
        nonce: String,
        wake: File,
        llm: Arc<Llm>,
    ) -> Shared {
        let wake = Arc::new(wake);
        let tasks = TaskMaker {
            stdout: Arc::new(stdout.try_clone().expect("dup stdout")),
            wake: wake.clone(),
        };
        let env = Env {
            llm: llm.clone(),
            tasks: tasks.clone(),
            tracker,
            resolver: Resolver::default(),
            history: SharedHistory::new(Box::new(MemHistory::default())),
        };
        let mut modules = module::builtin(enabled);
        // A late plugin result writes a byte to the wake pipe; the input thread
        // polls it next to stdin and calls `poll_plugins` (no lock taken here).
        let mut b = Editor::builder().theme(Theme::from_env()).waker(move || {
            let _ = (&*wake).write(&[1]);
        });
        for m in &mut modules {
            for p in m.editor_plugins(&env) {
                b = b.with_boxed(p);
            }
        }
        Arc::new(Mutex::new(Controller {
            ed: b.build(),
            scan: Scan::new(nonce),
            prompt: false,
            dirty: false,
            dsr: None,
            dsr_filter: CursorReportFilter::default(),
            use_dsr: true,
            disabled: false,
            outbox: Vec::new(),
            stdout,
            modules,
            events: Vec::with_capacity(8),
            notice: Vec::new(),
            seen_blocks: 0,
            llm,
            tasks,
            task: None,
            pending_prefill: None,
            typeahead: Vec::new(),
        }))
    }

    pub fn in_prompt(&self) -> bool {
        self.prompt
    }

    /// Prompt printed, nothing typed into bash's line, not yet ours.
    pub fn armed(&self) -> bool {
        self.scan.b_seen
            && self.task.is_none()
            && !self.prompt
            && !self.dirty
            && !self.disabled
            && !self.scan.printed_after_b
    }

    /// Stop using prompt mode for the rest of the session (after a panic).
    pub fn disable(&mut self) {
        self.disabled = true;
        if self.prompt {
            self.erase();
            self.leave(false);
        }
    }

    /// Bytes to type into bash; write them after releasing the lock.
    pub fn take_outbox(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.outbox)
    }

    /// A command block finished (called from the output thread).
    pub fn on_block(&mut self, total: u64, block: &Block) {
        if total != self.seen_blocks {
            self.seen_blocks = total;
            for m in &mut self.modules {
                m.on_block_finished(block);
            }
        }
    }

    fn draw(&mut self) {
        let _ = self.ed.draw(&mut self.stdout, width());
        let _ = self.stdout.flush();
    }

    fn erase(&mut self) {
        let _ = self.ed.erase(&mut self.stdout);
    }

    /// Output that is going to the terminal while we are NOT in prompt mode;
    /// call after writing it. Returns whether the quiet timer should run.
    pub fn after_passthrough_output(&mut self, data: &[u8]) -> bool {
        self.scan.feed(data, width());
        if std::mem::take(&mut self.scan.saw_cd) {
            self.dirty = false;
        }
        self.scan.saw_a = false;
        self.scan.saw_b = false;
        self.armed()
    }

    /// Output arrived while we own the line: step aside, show it, come back.
    pub fn output_in_prompt(&mut self, data: &[u8]) -> bool {
        self.erase();
        let _ = self.stdout.write_all(data);
        let w = width();
        self.scan.feed(data, w);
        if self.scan.saw_a || self.scan.saw_b || self.scan.saw_cd {
            // bash is redrawing its prompt (A..B, or B alone after a resize of a
            // multi-line prompt) or running something: let go and pick the line
            // up again at the next quiet B.
            self.scan.saw_a = false;
            self.scan.saw_b = false;
            if std::mem::take(&mut self.scan.saw_cd) {
                self.dirty = false;
            }
            self.leave(false);
            return self.armed();
        }
        let col = self.scan.column(w);
        if col != 0 {
            let _ = self.stdout.write_all(b"\r\n");
            self.scan.col = 0;
        }
        // Stray output (background job...): ask bash to repaint its prompt below
        // it (private key bound to redraw-current-line); the repaint carries
        // OSC 133 A/B so we pick the line up again like at any prompt.
        self.scan.b_seen = false;
        self.leave(false);
        self.send(REDRAW_KEY);
        false
    }

    /// The quiet timer fired.
    pub fn on_quiet(&mut self) {
        if !self.armed() {
            return;
        }
        if let Some(n) = self.llm.take_notice() {
            // bash's prompt is on this line: put the notice in its place and
            // have bash repaint the prompt below (picked up at the next quiet).
            let th = self.ed.theme();
            let (a, b) = if th.enabled {
                (format!("\x1b[{}m", th.dim), "\x1b[0m")
            } else {
                (String::new(), "")
            };
            let _ = write!(self.stdout, "\r\x1b[2K{a}{n}{b}\r\n");
            self.scan.col = 0;
            self.scan.b_seen = false;
            self.send(REDRAW_KEY);
            return;
        }
        for m in &mut self.modules {
            m.on_prompt();
        }
        self.prompt = true;
        let w = width();
        self.ed.reset_screen(self.scan.column(w));
        // A command from the model must wait for a fresh Enter: keys typed
        // while the answer streamed are dropped when there is one.
        if let Some(p) = self.pending_prefill.take() {
            self.ed.set_text(&p);
            self.typeahead.clear();
        }
        let _ = self.stdout.write_all(b"\x1b[?2004h");
        if self.use_dsr {
            let _ = self.stdout.write_all(b"\x1b[6n");
            self.dsr = Some(Instant::now());
        }
        self.draw();
        if !self.typeahead.is_empty() {
            let mut ta = std::mem::take(&mut self.typeahead);
            let n = ta.len();
            self.on_input(&mut ta, n);
        }
    }

    /// Leave prompt mode. `sent`: we are handing control to bash (a line,
    /// ^C, ^D, ^L was sent), so its prompt is no longer waiting.
    fn leave(&mut self, sent: bool) {
        self.prompt = false;
        // A pending cursor-position reply is still filtered from the input
        // (it must not leak to bash or the next program); `dsr` expires by TTL.
        if sent {
            self.scan.b_seen = false;
            let _ = self.stdout.write_all(b"\x1b[?2004l");
        }
    }

    /// Bytes from the keyboard. `buf[..n]` is valid; passthrough may rewrite it.
    pub fn on_input(&mut self, buf: &mut [u8], n: usize) -> Input {
        if let Some(t) = &self.task {
            let mut data = buf[..n].to_vec();
            if self.dsr.is_some() {
                let mut out = Vec::with_capacity(n);
                if self.dsr_filter.filter(&buf[..n], &mut out) {
                    self.dsr = None;
                    out.extend(self.dsr_filter.take_held());
                }
                data = out;
            }
            for b in data {
                if b == 0x03 {
                    t.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                } else if self.typeahead.len() < 65536 {
                    self.typeahead.push(b);
                }
            }
            return Input::Handled(false);
        }
        if !self.prompt {
            if let Some(t) = self.dsr {
                let mut out = Vec::with_capacity(n);
                let found = self.dsr_filter.filter(&buf[..n], &mut out);
                if found || t.elapsed() > DSR_TTL {
                    self.dsr = None;
                    out.extend(self.dsr_filter.take_held());
                }
                if self.scan.b_seen && !out.is_empty() {
                    self.dirty = out[out.len() - 1] != 0x03;
                }
                return Input::ForwardOwned(out);
            }
            if self.scan.b_seen && n > 0 {
                self.dirty = buf[n - 1] != 0x03;
            }
            return Input::Forward(n);
        }
        let mut ev = std::mem::take(&mut self.events);
        self.ed.input(&buf[..n], &mut ev);
        self.ed.poll(&mut ev);
        self.handle_events(&mut ev);
        self.events = ev;
        Input::Handled(self.ed.has_pending_input())
    }

    /// Input went quiet while a partial key was pending.
    pub fn flush_keys(&mut self) {
        let mut ev = std::mem::take(&mut self.events);
        self.ed.flush_input(&mut ev);
        if self.prompt {
            self.handle_events(&mut ev);
        }
        self.events = ev;
    }

    /// A plugin delivered something (async completion...).
    pub fn poll_plugins(&mut self) {
        if let Some(done) = self.task.as_ref().and_then(|t| t.take_done()) {
            // The answer is over: bash gets Enter on its empty line and
            // prints a fresh prompt, where the editor takes over again.
            self.task = None;
            self.pending_prefill = done;
            self.send(b"\r");
        }
        if !self.prompt {
            return;
        }
        let mut ev = std::mem::take(&mut self.events);
        self.ed.poll(&mut ev);
        self.handle_events(&mut ev);
        self.events = ev;
    }

    fn send(&mut self, bytes: &[u8]) {
        self.outbox.extend_from_slice(bytes);
    }

    fn handle_events(&mut self, events: &mut Vec<Event>) {
        let mut left = false;
        for ev in events.drain(..) {
            match ev {
                Event::CursorReport(_, c) => {
                    if self.dsr.take().is_some() && !left && c >= 1 {
                        let col = (c - 1).min(width() - 1);
                        if col != self.ed.start_col() {
                            self.erase();
                            self.ed.reset_screen(col);
                        }
                    }
                }
                Event::NeedRedraw => {}
                Event::Submit(line) => {
                    if !left {
                        self.erase();
                        self.leave(true);
                        left = true;
                    }
                    self.send_line(&line);
                }
                Event::Intercepted(line) => {
                    if left {
                        continue;
                    }
                    let mut notice = std::mem::take(&mut self.notice);
                    notice.clear();
                    let mut task = Some(self.tasks.make(self.ed.theme(), width()));
                    let shared = task.as_ref().map(|t| t.shared.clone());
                    let handled = self
                        .modules
                        .iter_mut()
                        .any(|m| m.on_intercept(&line, &mut notice, &mut task));
                    if handled && task.is_none() {
                        // Answer comes from a worker thread; it writes the rows.
                        let _ = self.ed.draw_final(&mut self.stdout, width(), &line);
                        self.leave(true);
                        self.task = shared;
                    } else if handled {
                        // The typed line stays on screen, the notice follows,
                        // and a bare Enter makes bash print a fresh prompt (its
                        // line is empty: an empty block, which the tracker drops).
                        let _ = self.ed.draw_final(&mut self.stdout, width(), &line);
                        let _ = self.stdout.write_all(b"\r\n");
                        let dim = self.ed.theme().enabled;
                        if dim {
                            let _ = write!(self.stdout, "\x1b[{}m", self.ed.theme().dim);
                        }
                        let _ = self.stdout.write_all(&notice);
                        if dim {
                            let _ = self.stdout.write_all(b"\x1b[0m");
                        }
                        self.leave(true);
                        self.send(b"\r");
                    } else {
                        self.erase();
                        self.leave(true);
                        self.send_line(&line);
                    }
                    self.notice = notice;
                    left = true;
                }
                Event::PassKey(b) => {
                    if !left {
                        self.erase();
                        self.leave(true);
                        left = true;
                    }
                    self.send(&b);
                }
                _ => {}
            }
        }
        if !left && self.prompt {
            self.draw();
        }
    }

    /// Type `line` into bash and press Enter. With bracketed paste on in bash
    /// the line goes in as one paste (newlines stay literal, so a multi-line
    /// line is one history entry); otherwise control characters are ^V-quoted.
    fn send_line(&mut self, line: &str) {
        let mut b = Vec::with_capacity(line.len() + 16);
        if self.scan.bracketed {
            b.extend_from_slice(b"\x1b[200~");
            b.extend(line.bytes().filter(|&c| c != 0x1b));
            b.extend_from_slice(b"\x1b[201~");
        } else {
            for &c in line.as_bytes() {
                if (c < 0x20 && c != b'\n') || c == 0x7f {
                    b.push(0x16);
                }
                b.push(c);
            }
        }
        b.push(b'\r');
        self.send(&b);
    }

    /// Terminal resized: bash gets the new size via `apply`; we erase before
    /// and redraw after so the region is consistent with what the terminal shows.
    pub fn resize(&mut self, apply: impl FnOnce()) {
        if self.prompt {
            // The terminal already reflowed our rows: erase with the new width.
            let _ = self.ed.erase_resized(&mut self.stdout, width());
        }
        apply();
        if self.prompt {
            self.draw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: &[u8] = b"\x1b]133;B;llmsh=n1\x07";

    fn ctl() -> Shared {
        let null = || File::create("/dev/null").unwrap();
        Controller::start(
            null(),
            Arc::new(Mutex::new(BlockTracker::default())),
            Some(&[]),
            "n1".into(),
            null(),
            Llm::new(),
        )
    }

    fn at_prompt(c: &Shared) {
        let (armed, _) = with(c, false, |g| g.after_passthrough_output(B));
        assert!(armed);
        with(c, (), |g| g.on_quiet());
        assert!(lock(c).in_prompt());
    }

    #[test]
    fn foreign_osc133_does_not_start_prompt_mode() {
        let c = ctl();
        // ssh/docker/nested shell inside a running command: untagged marks
        let (armed, _) = with(&c, true, |g| {
            g.after_passthrough_output(b"\x1b]133;A\x07$ \x1b]133;B\x07")
        });
        assert!(!armed);
        with(&c, (), |g| g.on_quiet());
        assert!(!lock(&c).in_prompt());
    }

    #[test]
    fn type_ahead_echo_keeps_prompt_native() {
        let c = ctl();
        // readline redisplays the type-ahead after B: its line is not empty
        with(&c, (), |g| {
            g.after_passthrough_output(B);
            g.after_passthrough_output(b"ls");
            g.on_quiet();
        });
        assert!(!lock(&c).in_prompt());
    }

    #[test]
    fn big_submit_goes_to_outbox_not_the_pty_under_lock() {
        let c = ctl();
        at_prompt(&c);
        let line = "x".repeat(300_000);
        let mut buf = format!("{line}\r").into_bytes();
        let n = buf.len();
        let (r, out) = with(&c, Input::Forward(n), |g| g.on_input(&mut buf, n));
        assert!(matches!(r, Input::Handled(_)));
        // typed into bash in one piece, written by the caller after unlocking
        assert!(out.len() > line.len() && out.ends_with(b"\r"));
        assert!(!lock(&c).in_prompt());
    }

    #[test]
    fn cursor_report_is_filtered_after_leaving_prompt_mode() {
        let c = ctl();
        at_prompt(&c);
        // Enter before the terminal answered the DSR
        let mut b = b"\r".to_vec();
        with(&c, (), |g| {
            g.on_input(&mut b, 1);
        });
        let mut reply = b"\x1b[3;7R".to_vec();
        let n = reply.len();
        let (r, _) = with(&c, Input::Forward(n), |g| g.on_input(&mut reply, n));
        match r {
            Input::ForwardOwned(v) => assert!(v.is_empty(), "{v:?}"),
            _ => panic!("reply must not be forwarded"),
        }
    }

    #[test]
    fn panic_disables_prompt_mode_instead_of_killing_input() {
        let c = ctl();
        at_prompt(&c);
        let (r, _) = with(&c, 7, |_| panic!("plugin bug"));
        assert_eq!(r, 7);
        assert!(!lock(&c).in_prompt());
        let (armed, _) = with(&c, true, |g| g.after_passthrough_output(B));
        assert!(!armed, "disabled for the rest of the session");
        let mut k = b"a".to_vec();
        let (r, _) = with(&c, Input::Handled(false), |g| g.on_input(&mut k, 1));
        assert!(matches!(r, Input::Forward(_) | Input::ForwardOwned(_)));
    }
}
