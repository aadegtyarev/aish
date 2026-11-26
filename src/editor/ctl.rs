//! I/O side of prompt mode: decides when we own the input line, feeds keys to
//! the editor, draws, and hands finished lines to bash.
//!
//! Lock discipline: one mutex around [`Controller`], taken by the input thread
//! (per read), the output thread (per chunk) and the resize thread. Bulk
//! passthrough writes happen outside the lock.

use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::highlight::{is_model_line, Resolver};
use super::history::History;
use super::keys::{strip_dsr, Decoder, Key};
use super::render::Renderer;
use super::scan::Scan;
use super::{Action, Editor};

/// Output silence after OSC 133;B before we take over the input line.
pub const QUIET: Duration = Duration::from_millis(10);
/// A lone ESC is an Escape key if nothing follows within this time.
pub const ESC_WAIT: Duration = Duration::from_millis(30);
const DSR_TTL: Duration = Duration::from_secs(1);

/// Bound to `redraw-current-line` in rc.bash.
const REDRAW_KEY: &[u8] = b"\x1b[1337~";
const MODEL_NOTICE: &str = " │ model not connected yet";

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
    rend: Renderer,
    scan: Scan,
    dec: Decoder,
    keys: Vec<Key>,
    prompt: bool,
    /// Bytes went to bash while its prompt was waiting: its line is not empty.
    dirty: bool,
    dsr: Option<Instant>,
    use_dsr: bool,
    master: File,
    stdout: File,
    out: Vec<u8>,
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
    /// Build the controller; history and PATH are loaded in the background.
    pub fn start(master: File, stdout: File) -> Shared {
        let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
        let ctl = Arc::new(Mutex::new(Controller {
            ed: Editor::new(color),
            rend: Renderer::default(),
            scan: Scan::default(),
            dec: Decoder::new(),
            keys: Vec::with_capacity(16),
            prompt: false,
            dirty: false,
            dsr: None,
            use_dsr: true,
            master,
            stdout,
            out: Vec::with_capacity(4096),
        }));
        let c = ctl.clone();
        std::thread::spawn(move || {
            let h = History::load();
            let (path, cmds) = Resolver::scan_path();
            let mut g = lock(&c);
            g.ed.hist = h;
            g.ed.resolver.set_path(path, cmds);
        });
        ctl
    }

    pub fn in_prompt(&self) -> bool {
        self.prompt
    }

    /// Prompt printed, nothing typed into bash's line, not yet ours.
    pub fn armed(&self) -> bool {
        self.scan.b_seen && !self.prompt && !self.dirty
    }

    fn flush_out(&mut self) {
        let _ = self.stdout.write_all(&self.out);
        self.out.clear();
    }

    fn draw(&mut self) {
        let w = width();
        self.ed.prepare(w);
        self.out.clear();
        self.rend.draw(&mut self.out, &self.ed.view(), w);
        self.flush_out();
    }

    fn erase(&mut self) {
        self.out.clear();
        self.rend.erase(&mut self.out);
        self.flush_out();
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
        if self.ed.resolver.path_stale() {
            let c_path = Resolver::scan_path();
            self.ed.resolver.set_path(c_path.0, c_path.1);
        }
        self.prompt = true;
        let w = width();
        self.rend.reset(self.scan.column(w));
        self.out.clear();
        self.out.extend_from_slice(b"\x1b[?2004h");
        if self.use_dsr {
            self.out.extend_from_slice(b"\x1b[6n");
            self.dsr = Some(Instant::now());
        }
        self.flush_out();
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
                let (l, found) = strip_dsr(&mut buf[..n]);
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
        let mut keys = std::mem::take(&mut self.keys);
        self.dec.feed(&buf[..n], &mut |k| keys.push(k));
        self.run_keys(&mut keys);
        self.keys = keys;
        Input::Handled(self.dec.has_pending())
    }

    /// Input went quiet while a partial key was pending.
    pub fn flush_keys(&mut self) {
        let mut keys = std::mem::take(&mut self.keys);
        self.dec.flush(&mut |k| keys.push(k));
        if self.prompt {
            self.run_keys(&mut keys);
        }
        self.keys = keys;
    }

    fn send(&mut self, bytes: &[u8]) {
        let _ = self.master.write_all(bytes);
    }

    fn run_keys(&mut self, keys: &mut Vec<Key>) {
        let mut it = keys.drain(..);
        while let Some(k) = it.next() {
            if let Key::CursorPos(_, c) = k {
                if self.dsr.take().is_some() && c >= 1 {
                    let col = (c - 1).min(width() - 1);
                    if col != self.rend.start_col {
                        self.erase();
                        self.rend.reset(col);
                    }
                }
                continue;
            }
            match self.ed.handle_key(k) {
                Action::None => {}
                a => {
                    self.finish(a);
                    // Whatever was typed after the line ended belongs to bash.
                    let rest: Vec<Key> = it.collect();
                    self.forward_keys(&rest);
                    return;
                }
            }
        }
        drop(it);
        self.draw();
    }

    fn forward_keys(&mut self, keys: &[Key]) {
        let mut b = Vec::new();
        for k in keys {
            match k {
                Key::Char(c) => b.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
                Key::Enter => b.push(b'\r'),
                Key::Tab => b.push(b'\t'),
                Key::Backspace => b.push(0x7f),
                Key::Paste(s) => b.extend_from_slice(s.as_bytes()),
                Key::Ctrl(c) if c.is_ascii_lowercase() => b.push(c - b'a' + 1),
                _ => {}
            }
        }
        if !b.is_empty() {
            self.send(&b);
        }
    }

    /// A key ended prompt mode.
    fn finish(&mut self, a: Action) {
        match a {
            Action::Submit => {
                let line = self.ed.take_line();
                if is_model_line(&line) {
                    self.model_line(&line);
                } else {
                    self.erase();
                    self.leave(true);
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
            }
            Action::Interrupt => {
                self.erase();
                self.leave(true);
                self.send(&[0x03]);
            }
            Action::Eof => {
                self.erase();
                self.leave(true);
                self.send(&[0x04]);
            }
            Action::ClearScreen => {
                self.erase();
                self.leave(true);
                self.send(&[0x0c]);
            }
            Action::None => {}
        }
    }

    /// Model lines are not sent to bash. The typed line stays on screen, a dim
    /// notice follows, and a bare Enter makes bash print a fresh prompt (its
    /// line is empty, so this records an empty block, which the tracker drops).
    fn model_line(&mut self, line: &str) {
        let w = width();
        self.ed.prepare(w);
        self.out.clear();
        // Draw the finished line without ghost/extra rows, cursor at its end.
        let mut v = self.ed.view();
        v.text = line;
        v.cursor = line.len();
        v.ghost = "";
        v.extra = b"";
        v.extra_rows = 0;
        self.rend.draw(&mut self.out, &v, w);
        self.out.extend_from_slice(b"\r\n");
        if self.ed.color {
            self.out.extend_from_slice(b"\x1b[2m");
        }
        self.out.extend_from_slice(MODEL_NOTICE.as_bytes());
        if self.ed.color {
            self.out.extend_from_slice(b"\x1b[0m");
        }
        self.flush_out();
        self.leave(true);
        self.send(b"\r");
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
