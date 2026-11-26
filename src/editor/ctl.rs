//! I/O side of prompt mode: decides when we own the input line, feeds bytes
//! to the editor, draws, and hands finished lines to bash.
//!
//! Lock discipline: one mutex around [`Controller`], taken by the input thread
//! (per read), the output thread (per chunk), the resize thread and plugin
//! wakers. Bulk passthrough writes happen outside the lock.

use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use lineedit_wip::plugins::SharedHistory;
use lineedit_wip::{strip_cursor_reports, Editor, Event, MemHistory, Theme};

use super::resolve::Resolver;
use super::scan::Scan;
use crate::blocks::{Block, BlockTracker};
use crate::module::{self, Env, Module};

/// Output silence after OSC 133;B before we take over the input line.
pub const QUIET: Duration = Duration::from_millis(10);
/// A lone ESC is an Escape key if nothing follows within this time.
pub const ESC_WAIT: Duration = Duration::from_millis(30);
const DSR_TTL: Duration = Duration::from_secs(1);

/// Bound to `redraw-current-line` in rc.bash.
const REDRAW_KEY: &[u8] = b"\x1b[1337~";

pub type Shared = Arc<Mutex<Controller>>;

pub fn lock(s: &Shared) -> MutexGuard<'_, Controller> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

pub enum Input {
    /// Send the first N bytes of the buffer to bash untouched.
    Forward(usize),
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
    use_dsr: bool,
    master: File,
    stdout: File,
    modules: Vec<Box<dyn Module>>,
    events: Vec<Event>,
    notice: Vec<u8>,
    seen_blocks: u64,
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
        master: File,
        stdout: File,
        tracker: Arc<Mutex<BlockTracker>>,
        enabled: Option<&[String]>,
    ) -> Shared {
        let env = Env {
            tracker,
            resolver: Resolver::default(),
            history: SharedHistory::new(Box::new(MemHistory::default())),
        };
        let mut modules = module::builtin(enabled);
        // Late plugin results wake us through this slot (set once `ctl` exists).
        let slot: Arc<OnceLock<Weak<Mutex<Controller>>>> = Arc::new(OnceLock::new());
        let waker_slot = slot.clone();
        let mut b = Editor::builder().theme(Theme::from_env()).waker(move || {
            let Some(c) = waker_slot.get().and_then(Weak::upgrade) else {
                return;
            };
            // The thread that delivers is never the one holding the lock;
            // if it is anyway (synchronous delivery), the next key polls.
            for _ in 0..50 {
                if let Ok(mut g) = c.try_lock() {
                    g.poll_plugins();
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        for m in &mut modules {
            for p in m.editor_plugins(&env) {
                b = b.with_boxed(p);
            }
        }
        let ctl = Arc::new(Mutex::new(Controller {
            ed: b.build(),
            scan: Scan::default(),
            prompt: false,
            dirty: false,
            dsr: None,
            use_dsr: true,
            master,
            stdout,
            modules,
            events: Vec::with_capacity(8),
            notice: Vec::new(),
            seen_blocks: 0,
        }));
        let _ = slot.set(Arc::downgrade(&ctl));
        ctl
    }

    pub fn in_prompt(&self) -> bool {
        self.prompt
    }

    /// Prompt printed, nothing typed into bash's line, not yet ours.
    pub fn armed(&self) -> bool {
        self.scan.b_seen && !self.prompt && !self.dirty
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
        self.armed()
    }

    /// Output arrived while we own the line: step aside, show it, come back.
    pub fn output_in_prompt(&mut self, data: &[u8]) -> bool {
        self.erase();
        let _ = self.stdout.write_all(data);
        let w = width();
        self.scan.feed(data, w);
        if self.scan.saw_a || self.scan.saw_cd {
            // bash is redrawing its prompt (or running something): let go and
            // pick the line up again at the next quiet B.
            self.scan.saw_a = false;
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
        for m in &mut self.modules {
            m.on_prompt();
        }
        self.prompt = true;
        let w = width();
        self.ed.reset_screen(self.scan.column(w));
        let _ = self.stdout.write_all(b"\x1b[?2004h");
        if self.use_dsr {
            let _ = self.stdout.write_all(b"\x1b[6n");
            self.dsr = Some(Instant::now());
        }
        self.draw();
    }

    /// Leave prompt mode. `sent`: we are handing control to bash (a line,
    /// ^C, ^D, ^L was sent), so its prompt is no longer waiting.
    fn leave(&mut self, sent: bool) {
        self.prompt = false;
        self.dsr = None;
        if sent {
            self.scan.b_seen = false;
            let _ = self.stdout.write_all(b"\x1b[?2004l");
        }
    }

    /// Bytes from the keyboard. `buf[..n]` is valid; passthrough may rewrite it.
    pub fn on_input(&mut self, buf: &mut [u8], n: usize) -> Input {
        if !self.prompt {
            let mut len = n;
            if let Some(t) = self.dsr {
                let (l, found) = strip_cursor_reports(&mut buf[..n]);
                len = l;
                if found || t.elapsed() > DSR_TTL {
                    self.dsr = None;
                }
            }
            if self.scan.b_seen && len > 0 {
                self.dirty = buf[len - 1] != 0x03;
            }
            return Input::Forward(len);
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
        if !self.prompt {
            return;
        }
        let mut ev = std::mem::take(&mut self.events);
        self.ed.poll(&mut ev);
        self.handle_events(&mut ev);
        self.events = ev;
    }

    fn send(&mut self, bytes: &[u8]) {
        let _ = self.master.write_all(bytes);
    }

    fn handle_events(&mut self, events: &mut Vec<Event>) {
        let mut left = false;
        for ev in events.drain(..) {
            match ev {
                Event::CursorReport(_, c) => {
                    if !left && self.dsr.take().is_some() && c >= 1 {
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
                    let handled = self
                        .modules
                        .iter_mut()
                        .any(|m| m.on_intercept(&line, &mut notice));
                    if handled {
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
            }
        }
        if !left && self.prompt {
            self.draw();
        }
    }

    /// Type `line` into bash and press Enter.
    fn send_line(&mut self, line: &str) {
        let mut b = Vec::with_capacity(line.len() + 1);
        for &c in line.as_bytes() {
            // Quote controls so readline inserts them literally.
            if (c < 0x20 && c != b'\n') || c == 0x7f {
                b.push(0x16);
            }
            b.push(c);
        }
        b.push(b'\r');
        self.send(&b);
    }

    /// Terminal resized: bash gets the new size via `apply`; we erase before
    /// and redraw after so the region is consistent with what the terminal shows.
    pub fn resize(&mut self, apply: impl FnOnce()) {
        if self.prompt {
            self.erase();
        }
        apply();
        if self.prompt {
            self.draw();
        }
    }
}
