//! The Ctrl-Q overlay: ask the model while a command is running.
//!
//! The key is spotted in the keyboard stream on its way to the program
//! ([`Detector`], no allocation on the normal path). The input thread then
//! takes over: [`Gate`] parks the output thread (the program blocks on its
//! writes, nothing is lost), the overlay is drawn on the alternate screen and
//! driven from a small blocking loop ([`run`]); on close the screen is given
//! back, the output thread is released and, if asked, a command is typed into
//! the program (never followed by Enter).

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use lineedit_wip::{Editor, Event, Theme};

use crate::blocks::BlockTracker;
use crate::config::Setting;
use crate::llm::ask::{self, Screen};
use crate::llm::render::{sanitize, unsafe_cmd, Renderer};
use crate::llm::Llm;
use crate::module::{TaskMaker, TaskShared};

/// The key that asks the model: Ctrl plus a letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AskKey {
    letter: u8,
}

impl AskKey {
    pub const DEFAULT: AskKey = AskKey { letter: b'q' };

    /// `ctrl-<letter>` (case-insensitive, `ctrl+x` too) or `off`.
    pub fn parse(s: &str) -> Result<Option<AskKey>, String> {
        let t = s.trim().to_ascii_lowercase();
        if t == "off" {
            return Ok(None);
        }
        let l = t
            .strip_prefix("ctrl-")
            .or_else(|| t.strip_prefix("ctrl+"))
            .filter(|l| l.len() == 1)
            .map(|l| l.as_bytes()[0])
            .filter(u8::is_ascii_lowercase)
            .ok_or_else(|| format!("keys.ask: '{s}' is not ctrl-<letter> or off"))?;
        // Tab, Enter, Backspace, Ctrl-C and Ctrl-J cannot be given away
        if matches!(l, b'c' | b'h' | b'i' | b'j' | b'm') {
            return Err(format!(
                "keys.ask: ctrl-{} is taken by the terminal",
                l as char
            ));
        }
        Ok(Some(AskKey { letter: l }))
    }

    /// The configured key; a bad value is logged and the default is used.
    pub fn from_config(s: &Setting<String>) -> Option<AskKey> {
        match s {
            Setting::Unset => Some(AskKey::DEFAULT),
            Setting::Off => None,
            Setting::Value(v) => AskKey::parse(v).unwrap_or_else(|e| {
                tracing::warn!("{e}");
                Some(AskKey::DEFAULT)
            }),
        }
    }

    /// The editor key bound to the same press.
    pub fn editor_key(self) -> lineedit_wip::Key {
        lineedit_wip::Key::Ctrl(self.letter)
    }

    /// The control byte (0x11 for ctrl-q).
    pub fn byte(self) -> u8 {
        self.letter - b'a' + 1
    }

    /// The first press in `buf`: the raw byte or the kitty/CSI-u form
    /// (`ESC [ 113 ; 5 u`). Returns its byte range. Stateless: sequences
    /// split across reads are the [`Detector`]'s job.
    pub fn find(self, buf: &[u8]) -> Option<(usize, usize)> {
        let b = self.byte();
        buf.iter().enumerate().find_map(|(i, &c)| {
            if c == b {
                Some((i, i + 1))
            } else if c == 0x1b {
                match self.csi_u(&buf[i..]) {
                    M::Full(n) => Some((i, i + n)),
                    _ => None,
                }
            } else {
                None
            }
        })
    }

    fn csi_u(self, d: &[u8]) -> M {
        let l = self.letter;
        let pre = [
            0x1b,
            b'[',
            b'0' + l / 100,
            b'0' + l / 10 % 10,
            b'0' + l % 10,
            b';',
            b'5',
        ];
        let n = d.len().min(pre.len());
        if d[..n] != pre[..n] {
            return M::No;
        }
        if d.len() <= pre.len() {
            return M::Partial;
        }
        match &d[pre.len()..] {
            [b'u', ..] => M::Full(pre.len() + 1),
            [b':'] | [b':', b'1'] => M::Partial,
            [b':', b'1', b'u', ..] => M::Full(pre.len() + 3),
            _ => M::No,
        }
    }
}

enum M {
    No,
    Partial,
    Full(usize),
}

/// A press of the ask key found in the keyboard stream.
#[derive(Debug, PartialEq, Eq)]
pub struct AskReq {
    /// Bytes typed before the key: they belong to the program.
    pub before: Vec<u8>,
    /// Bytes typed after it: the first input of the overlay.
    pub after: Vec<u8>,
    /// The key as the terminal sent it (what "a literal Ctrl-Q" is).
    pub key: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Scan {
    /// Nothing found, nothing held: forward the bytes untouched.
    Clear,
    /// Forward these bytes instead (some bytes may be held back).
    Forward(Vec<u8>),
    Hit(AskReq),
}

/// Finds the ask key in bytes bound for the program. The common path is one
/// pass over the chunk without allocation. Only the start of a CSI-u
/// sequence at the very end of a read (three bytes or more, so a lone Escape
/// is never delayed) is held back until the next read or the Esc timeout.
#[derive(Default)]
pub struct Detector {
    held: Vec<u8>,
}

impl Detector {
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Bytes that were waiting for the rest of a sequence (Esc timeout).
    pub fn take_held(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }

    pub fn scan(&mut self, key: AskKey, buf: &[u8]) -> Scan {
        let b = key.byte();
        if self.held.is_empty() && !buf.iter().any(|&c| c == b || c == 0x1b) {
            return Scan::Clear;
        }
        let had_held = !self.held.is_empty();
        let joined;
        let data: &[u8] = if had_held {
            joined = [&self.held[..], buf].concat();
            &joined
        } else {
            buf
        };
        let mut hold_at = None;
        for (i, &c) in data.iter().enumerate() {
            let n = if c == b {
                1
            } else if c == 0x1b {
                match key.csi_u(&data[i..]) {
                    M::Full(n) => n,
                    M::Partial if data.len() - i >= 3 => {
                        hold_at = Some(i);
                        break;
                    }
                    _ => continue,
                }
            } else {
                continue;
            };
            self.held.clear();
            return Scan::Hit(AskReq {
                before: data[..i].to_vec(),
                after: data[i + n..].to_vec(),
                key: data[i..i + n].to_vec(),
            });
        }
        match hold_at {
            Some(h) => {
                let fwd = data[..h].to_vec();
                self.held = data[h..].to_vec();
                Scan::Forward(fwd)
            }
            None if had_held => {
                let all = data.to_vec();
                self.held.clear();
                Scan::Forward(all)
            }
            None => Scan::Clear,
        }
    }
}

/// What to type into the program for a command from the model, or `None`
/// when it must only be shown: a multi-line command needs bracketed paste
/// (otherwise its newlines would act as Enter).
pub fn insert_bytes(cmd: &str, bracketed: bool) -> Option<Vec<u8>> {
    let c = cmd.trim_end();
    if c.is_empty() || unsafe_cmd(c) {
        return None;
    }
    if !c.contains('\n') {
        return Some(c.as_bytes().to_vec());
    }
    bracketed.then(|| {
        let mut v = b"\x1b[200~".to_vec();
        v.extend_from_slice(c.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    })
}

/// Toggle the `? ` marker at the start of the line (the editor action).
/// Returns the new text and cursor; the cursor stays on the same character.
pub fn toggle_prefix(text: &str, cursor: usize) -> (String, usize) {
    if let Some(rest) = text
        .strip_prefix("? ")
        .or_else(|| text.strip_prefix('?').filter(|r| r.is_empty()))
    {
        let cut = text.len() - rest.len();
        (rest.to_owned(), cursor.saturating_sub(cut))
    } else {
        (format!("? {text}"), cursor + 2)
    }
}

// ---------------------------------------------------------------------------

#[derive(Default)]
struct GateSt {
    open: bool,
    busy: bool,
}

/// Parks the output thread while the overlay is open. The output thread
/// holds a [`Busy`] while it handles a chunk; opening waits until it is idle,
/// so the screen and mode state are exact and nothing is drawn halfway.
#[derive(Default)]
pub struct Gate {
    st: Mutex<GateSt>,
    cv: Condvar,
    /// The terminal was resized while the overlay is open.
    pub resized: AtomicBool,
    /// Bytes that put the terminal back; taken by whoever leaves first.
    restore: Mutex<Option<Vec<u8>>>,
}

pub struct Busy<'a>(&'a Gate);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.state().busy = false;
        self.0.cv.notify_all();
    }
}

impl Gate {
    fn state(&self) -> MutexGuard<'_, GateSt> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Output thread: wait while the overlay is open, then mark busy.
    pub fn enter(&self) -> Busy<'_> {
        let mut g = self.state();
        while g.open {
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
        g.busy = true;
        Busy(self)
    }

    /// Input thread: stop the output thread (call without the controller lock).
    pub fn open(&self) {
        let mut g = self.state();
        g.open = true;
        while g.busy {
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn close(&self) {
        self.state().open = false;
        self.cv.notify_all();
    }

    pub fn is_open(&self) -> bool {
        self.state().open
    }

    fn set_restore(&self, b: Vec<u8>) {
        *self.restore.lock().unwrap_or_else(|e| e.into_inner()) = Some(b);
    }

    fn take_restore(&self) -> Option<Vec<u8>> {
        self.restore
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// The process is going away with the overlay open: give the screen back.
    pub fn emergency_restore(&self) {
        if let Some(b) = self.take_restore() {
            // SAFETY: plain write(2) of a live buffer to stdout.
            unsafe { nix::libc::write(1, b.as_ptr().cast(), b.len()) };
        }
    }
}

/// Bytes that return the terminal to the program: leave our alternate screen
/// (or clear the program's own), and bracketed paste as the program had it.
fn restore_bytes(alt: bool, bracketed: bool) -> Vec<u8> {
    let mut v = b"\x1b[0m".to_vec();
    v.extend_from_slice(if alt {
        b"\x1b[2J\x1b[H"
    } else {
        b"\x1b[?1049l"
    });
    v.extend_from_slice(if bracketed {
        b"\x1b[?2004h"
    } else {
        b"\x1b[?2004l"
    });
    v
}

/// Make a program in the alternate screen redraw: its window size changes
/// and changes back (which ssh passes on as a window-change).
fn nudge(master: &File) {
    let mut ws = crate::term::window_size();
    let rows = ws.ws_row;
    if rows < 2 {
        return;
    }
    ws.ws_row = rows - 1;
    crate::term::set_window_size(master.as_raw_fd(), &ws);
    std::thread::sleep(Duration::from_millis(40));
    ws.ws_row = rows;
    crate::term::set_window_size(master.as_raw_fd(), &ws);
}

// ---------------------------------------------------------------------------

/// Everything the overlay needs; made under the controller lock.
pub struct Setup {
    pub key: AskKey,
    pub llm: Arc<Llm>,
    pub tracker: Arc<Mutex<BlockTracker>>,
    pub tasks: TaskMaker,
    pub theme: Theme,
    pub stdout: File,
    pub gate: Arc<Gate>,
    /// The program is in the alternate screen.
    pub alt: bool,
    /// The program turned bracketed paste on.
    pub bracketed: bool,
    pub screen: Screen,
}

enum St {
    /// Typing a question.
    Edit,
    /// The answer streams in; keys are ignored except Ctrl-C and Esc.
    Streaming { shared: Arc<TaskShared> },
    /// Done: `insert` is what Enter types into the program.
    Answered { insert: Option<Vec<u8>> },
}

/// Close the overlay, typing these bytes into the program.
type Close = Option<Vec<u8>>;

struct Overlay {
    s: Setup,
    ed: Editor,
    st: St,
    cancelled: bool,
    /// Lines of output the model gets (shown in the header).
    lines: usize,
    last_q: String,
    /// The terminal was resized while an answer was streaming.
    stale: bool,
}

fn cols() -> u16 {
    crate::term::window_size().ws_col.max(1)
}

fn is_enter(b: &[u8]) -> bool {
    matches!(b, [b'\r'] | [b'\n'] | [b'\r', b'\n'])
}

fn is_esc(b: &[u8]) -> bool {
    b == b"\x1b" || b == b"\x1b[27u"
}

fn is_ctrl_c(b: &[u8]) -> bool {
    b.contains(&3) || b.windows(7).any(|w| w == b"\x1b[99;5u")
}

/// The dim context line: the command and how much output the model sees.
pub fn header(cmd: Option<&str>, lines: usize, alt: bool, cols: usize) -> String {
    let one: String = sanitize(cmd.unwrap_or("команда"))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let meta = if alt {
        "полноэкранное приложение".to_owned()
    } else if lines == 0 {
        "вывода нет".to_owned()
    } else {
        format!("последние {lines} строк")
    };
    let max = cols.saturating_sub(2 + 3 + meta.chars().count()).max(8);
    let c = if one.chars().count() > max {
        let t: String = one.chars().take(max - 1).collect();
        format!("{t}…")
    } else {
        one
    };
    format!("  {c} · {meta}")
}

impl Overlay {
    fn w(&mut self, b: &[u8]) {
        let _ = self.s.stdout.write_all(b);
        let _ = self.s.stdout.flush();
    }

    fn dim(&self, t: &str) -> String {
        if self.s.theme.enabled {
            format!("\x1b[{}m{t}\x1b[0m", self.s.theme.dim)
        } else {
            t.to_owned()
        }
    }

    fn header_row(&self) -> String {
        let h = header(
            self.s.screen.cmd.as_deref(),
            self.lines,
            self.s.alt,
            cols() as usize,
        );
        self.dim(&h)
    }

    /// Clear the screen and draw the header and the input line.
    fn frame(&mut self) {
        let h = self.header_row();
        self.w(format!("\x1b[0m\x1b[2J\x1b[H{h}\r\n? ").as_bytes());
        self.ed.reset_screen(2);
        let _ = self.ed.draw(&mut self.s.stdout, cols());
        let _ = self.s.stdout.flush();
    }

    fn enter(&mut self) {
        self.s
            .gate
            .set_restore(restore_bytes(self.s.alt, self.s.bracketed));
        let mut o = Vec::new();
        if !self.s.alt {
            o.extend_from_slice(b"\x1b[?1049h");
        }
        o.extend_from_slice(b"\x1b[?2004h\x1b[?25h");
        self.w(&o);
        self.ed.clear();
        self.frame();
    }

    fn footer(&mut self, insert: bool) {
        let t = if insert {
            "  Enter — вставить · Esc — закрыть"
        } else {
            "  Esc — закрыть"
        };
        let d = self.dim(t);
        self.w(format!("\r\n{d}").as_bytes());
    }

    /// The answer is complete (or failed): show what can be done next.
    fn answered(&mut self, prefill: Option<String>) {
        let insert = prefill.and_then(|c| insert_bytes(&c, self.s.bracketed));
        self.footer(insert.is_some());
        self.st = St::Answered { insert };
        self.cancelled = false;
        if std::mem::take(&mut self.stale) {
            self.redraw();
        }
    }

    /// Redraw after a resize.
    fn redraw(&mut self) {
        match &self.st {
            St::Streaming { .. } => self.stale = true,
            St::Edit => self.frame(),
            St::Answered { insert } => {
                let has = insert.is_some();
                let h = self.header_row();
                let mut o =
                    format!("\x1b[0m\x1b[2J\x1b[H{h}\r\n? {}", sanitize(&self.last_q)).into_bytes();
                let text = self.s.llm.convo.lock().ok().and_then(|c| {
                    c.last()
                        .filter(|m| m.role == aish_llm::Role::Assistant)
                        .map(|m| m.text())
                });
                if let Some(t) = text {
                    let mut r = Renderer::new(&self.s.theme, cols());
                    r.push(&t, &mut o);
                    if let Some(c) = r.finish(&mut o, true) {
                        r.code_block(&c, &mut o);
                    }
                }
                self.w(&o);
                self.footer(has);
            }
        }
    }

    fn submit(&mut self, line: &str) {
        let q = line.trim().to_owned();
        let _ = self.ed.draw_final(&mut self.s.stdout, cols(), &q);
        let _ = self.s.stdout.flush();
        self.last_q = q.clone();
        let Some(target) = self.s.llm.target("chat") else {
            let msg = format!(
                "модель не настроена · {} config",
                self.s.llm.command_label()
            );
            let mut o = Vec::new();
            Renderer::new(&self.s.theme, cols()).note(&msg, &mut o);
            self.w(&o);
            self.answered(None);
            return;
        };
        let task = self.s.tasks.make(&self.s.theme, cols());
        let shared = task.shared.clone();
        let (llm, tracker, screen) = (
            self.s.llm.clone(),
            self.s.tracker.clone(),
            self.s.screen.clone(),
        );
        ask::run_task(task, move |t| {
            ask::overlay_answer(&llm, &tracker, &target, &screen, &q, t)
        });
        self.st = St::Streaming { shared };
        self.cancelled = false;
    }

    fn edit_bytes(&mut self, b: &[u8]) -> Close {
        let mut ev = Vec::new();
        self.ed.input(b, &mut ev);
        for e in ev {
            if let Event::Submit(line) = e {
                self.submit(&line);
                return None;
            }
        }
        let _ = self.ed.draw(&mut self.s.stdout, cols());
        let _ = self.s.stdout.flush();
        None
    }

    /// Keyboard input. `Some(bytes)` closes the overlay.
    fn on_bytes(&mut self, b: &[u8]) -> Close {
        if b.is_empty() {
            return None;
        }
        if is_esc(b) {
            return Some(Vec::new());
        }
        let hit = self.s.key.find(b);
        match &self.st {
            St::Streaming { shared } => {
                if is_ctrl_c(b) {
                    if self.cancelled {
                        return Some(Vec::new());
                    }
                    self.cancelled = true;
                    shared.cancel.store(true, Ordering::Relaxed);
                }
                None
            }
            St::Answered { insert } => {
                if is_enter(b) {
                    return insert.clone();
                }
                if hit.is_some() || is_ctrl_c(b) {
                    return Some(Vec::new());
                }
                // typing again asks a follow-up under the answer
                self.w(b"\r\x1b[2K? ");
                self.ed.reset_screen(2);
                self.ed.clear();
                self.st = St::Edit;
                self.edit_bytes(b)
            }
            St::Edit => {
                let empty = self.ed.text().is_empty();
                if let Some((s, e)) = hit {
                    if empty && s == 0 {
                        // the key again: close and hand a literal one to the program
                        return Some(b[s..e].to_vec());
                    }
                    let rest = [&b[..s], &b[e..]].concat();
                    return self.on_bytes(&rest);
                }
                if is_ctrl_c(b) {
                    if empty {
                        return Some(Vec::new());
                    }
                    self.ed.clear();
                    return self.edit_bytes(b"");
                }
                self.edit_bytes(b)
            }
        }
    }

    fn on_wake(&mut self) {
        if let St::Streaming { shared } = &self.st {
            if let Some(done) = shared.take_done() {
                self.answered(done);
            }
        }
        if self.s.gate.resized.swap(false, Ordering::Relaxed) {
            self.redraw();
        }
    }

    fn event_loop(&mut self, stdin: &File, wake: &File, first: &[u8]) -> Close {
        if let Some(c) = self.on_bytes(first) {
            return Some(c);
        }
        let mut buf = [0u8; 4096];
        loop {
            let (inp, woke) = wait2(stdin, wake);
            if woke {
                let mut sink = [0u8; 64];
                let _ = (&*wake).read(&mut sink);
                self.on_wake();
            }
            if inp {
                match (&*stdin).read(&mut buf) {
                    Ok(0) => return Some(Vec::new()),
                    Ok(n) => {
                        if let Some(c) = self.on_bytes(&buf[..n]) {
                            return Some(c);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return Some(Vec::new()),
                }
            }
        }
    }

    /// Give the terminal back (every exit path ends here).
    fn leave(&mut self, master: &File) {
        if let St::Streaming { shared } = &self.st {
            shared.muted.store(true, Ordering::Relaxed);
            shared.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(r) = self.s.gate.take_restore() {
            self.w(&r);
        }
        if self.s.alt {
            nudge(master);
        }
        self.s.gate.close();
    }
}

/// Wait for the keyboard or the wake pipe, retrying on signals.
fn wait2(a: &File, b: &File) -> (bool, bool) {
    use nix::errno::Errno;
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    loop {
        let mut fds = [
            PollFd::new(a.as_fd(), PollFlags::POLLIN),
            PollFd::new(b.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(n) if n > 0 => {
                let ready = |f: &PollFd| f.revents().is_some_and(|r| !r.is_empty());
                return (ready(&fds[0]), ready(&fds[1]));
            }
            Ok(_) | Err(Errno::EINTR) => {}
            Err(_) => return (true, false),
        }
    }
}

/// Run the overlay until it closes (blocking, on the input thread; the gate
/// is open). Returns false if the program's input is gone.
pub fn run(s: Setup, req: &AskReq, stdin: &File, wake: &File, master: &mut File) -> bool {
    let theme = s.theme.clone();
    let lines = ask::tail_lines(&s.screen.tail, ask::OVERLAY_LINES, usize::MAX).1;
    let mut ov = Overlay {
        ed: Editor::builder().theme(theme).build(),
        s,
        st: St::Edit,
        cancelled: false,
        lines,
        last_q: String::new(),
        stale: false,
    };
    crate::term::IN_GUARD.with(|x| x.set(true));
    let r = catch_unwind(AssertUnwindSafe(|| {
        ov.enter();
        ov.event_loop(stdin, wake, &req.after)
    }));
    let send = r.unwrap_or_else(|_| {
        tracing::error!("overlay panicked; resuming passthrough");
        Some(Vec::new())
    });
    let _ = catch_unwind(AssertUnwindSafe(|| ov.leave(master)));
    ov.s.gate.close();
    crate::term::IN_GUARD.with(|x| x.set(false));
    match send {
        Some(b) if !b.is_empty() => master.write_all(&b).is_ok(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: AskKey = AskKey { letter: b'q' };

    fn hit(s: Scan) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        match s {
            Scan::Hit(r) => (r.before, r.after, r.key),
            other => panic!("expected a hit, got {other:?}"),
        }
    }

    #[test]
    fn parse_key() {
        assert_eq!(AskKey::parse("ctrl-q"), Ok(Some(Q)));
        assert_eq!(AskKey::parse(" Ctrl+G "), Ok(Some(AskKey { letter: b'g' })));
        assert_eq!(AskKey::parse("off"), Ok(None));
        for bad in [
            "q", "ctrl-", "ctrl-qq", "alt-q", "ctrl-1", "ctrl-c", "ctrl-m", "ctrl-i",
        ] {
            assert!(AskKey::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Q.byte(), 0x11);
        assert_eq!(AskKey::from_config(&Setting::Unset), Some(Q));
        assert_eq!(AskKey::from_config(&Setting::Off), None);
        assert_eq!(AskKey::from_config(&Setting::Value("nope".into())), Some(Q));
    }

    #[test]
    fn raw_byte_and_csi_u() {
        let mut d = Detector::default();
        assert_eq!(d.scan(Q, b"hello"), Scan::Clear);
        assert_eq!(d.scan(Q, b"\x1b[A\x1b[1;5C"), Scan::Clear); // other keys
        assert_eq!(
            hit(d.scan(Q, b"ab\x11cd")),
            (b"ab".to_vec(), b"cd".to_vec(), vec![0x11])
        );
        assert_eq!(
            hit(d.scan(Q, b"\x1b[113;5u")),
            (vec![], vec![], b"\x1b[113;5u".to_vec())
        );
        assert_eq!(
            hit(d.scan(Q, b"x\x1b[113;5:1uy")),
            (b"x".to_vec(), b"y".to_vec(), b"\x1b[113;5:1u".to_vec())
        );
        // other letters / modifiers are not ours
        assert_eq!(d.scan(Q, b"\x1b[119;5u\x1b[113;3u\x1b[113u"), Scan::Clear);
        assert_eq!(AskKey::find(Q, b"\x1b[113;5u"), Some((0, 8)));
    }

    #[test]
    fn csi_u_split_across_reads() {
        let mut d = Detector::default();
        // everything but a lone Escape / `ESC [` is held back, then completed
        assert_eq!(d.scan(Q, b"ab\x1b[11"), Scan::Forward(b"ab".to_vec()));
        assert!(!d.is_empty());
        assert_eq!(
            hit(d.scan(Q, b"3;5uzz")),
            (vec![], b"zz".to_vec(), b"\x1b[113;5u".to_vec())
        );
        assert!(d.is_empty());
        // three pieces
        assert_eq!(d.scan(Q, b"\x1b[113"), Scan::Forward(vec![]));
        assert_eq!(d.scan(Q, b";5"), Scan::Forward(vec![]));
        assert_eq!(hit(d.scan(Q, b"u")).2, b"\x1b[113;5u");
        // the held start turns out to be something else: it is released
        assert_eq!(d.scan(Q, b"\x1b[11"), Scan::Forward(vec![]));
        assert_eq!(d.scan(Q, b"~x"), Scan::Forward(b"\x1b[11~x".to_vec()));
        assert!(d.is_empty());
        // Esc timeout releases what is held
        assert_eq!(d.scan(Q, b"\x1b[113;"), Scan::Forward(vec![]));
        assert_eq!(d.take_held(), b"\x1b[113;");
        assert!(d.is_empty());
        // a lone Escape (or `ESC [`) is never held: no added latency for vim
        assert_eq!(d.scan(Q, b"\x1b"), Scan::Clear);
        assert_eq!(d.scan(Q, b"\x1b["), Scan::Clear);
        assert!(d.is_empty());
    }

    #[test]
    fn insert_decision() {
        let ins = |c: &str, bp| insert_bytes(c, bp);
        // single line: typed as is, with and without bracketed paste, no newline
        assert_eq!(ins("ls -l\n", false), Some(b"ls -l".to_vec()));
        assert_eq!(ins("ls -l", true), Some(b"ls -l".to_vec()));
        // multi-line: only as a bracketed paste
        assert_eq!(ins("a \\\n  b", false), None);
        assert_eq!(
            ins("a \\\n  b", true),
            Some(b"\x1b[200~a \\\n  b\x1b[201~".to_vec())
        );
        // never Enter, never escapes
        assert_eq!(ins("", true), None);
        assert_eq!(ins("echo \x1b[31m", true), None);
        assert_eq!(ins("echo a\rb", true), None);
        for c in ["ls", "a\nb"] {
            let v = ins(c, true).unwrap();
            assert!(!v.ends_with(b"\n") && !v.ends_with(b"\r"));
        }
    }

    #[test]
    fn prefix_toggle() {
        assert_eq!(toggle_prefix("ls", 2), ("? ls".into(), 4));
        assert_eq!(toggle_prefix("ls", 0), ("? ls".into(), 2));
        assert_eq!(toggle_prefix("", 0), ("? ".into(), 2));
        assert_eq!(toggle_prefix("? ls", 4), ("ls".into(), 2));
        assert_eq!(toggle_prefix("? ls", 1), ("ls".into(), 0));
        assert_eq!(toggle_prefix("? ", 2), ("".into(), 0));
        assert_eq!(toggle_prefix("?", 1), ("".into(), 0));
        assert_eq!(toggle_prefix("?x", 2), ("? ?x".into(), 4)); // a glob stays a command
    }

    #[test]
    fn header_line() {
        assert_eq!(
            header(Some("ssh prod"), 200, false, 80),
            "  ssh prod · последние 200 строк"
        );
        assert_eq!(header(Some("cat"), 0, false, 80), "  cat · вывода нет");
        assert_eq!(
            header(None, 3, true, 80),
            "  команда · полноэкранное приложение"
        );
        let long = header(Some(&"x".repeat(200)), 5, false, 40);
        assert!(long.chars().count() <= 40 && long.contains('…'), "{long}");
        assert!(!header(Some("a\x1b[31m\tb"), 1, false, 80).contains('\x1b'));
    }

    #[test]
    fn restore_gives_the_screen_back() {
        assert_eq!(
            restore_bytes(false, false),
            b"\x1b[0m\x1b[?1049l\x1b[?2004l"
        );
        assert_eq!(
            restore_bytes(true, true),
            b"\x1b[0m\x1b[2J\x1b[H\x1b[?2004h"
        );
    }

    #[test]
    fn gate_waits_for_the_busy_chunk_and_parks_the_next() {
        let g = Arc::new(Gate::default());
        let busy = g.enter();
        let g2 = g.clone();
        let opener = std::thread::spawn(move || g2.open());
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !opener.is_finished(),
            "open() must wait for the chunk in flight"
        );
        drop(busy);
        opener.join().unwrap();
        assert!(g.is_open());
        let g3 = g.clone();
        let out = std::thread::spawn(move || drop(g3.enter()));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!out.is_finished(), "output stays parked while open");
        g.close();
        out.join().unwrap();
        assert!(!g.is_open());
    }

    #[test]
    fn keys_in_the_overlay() {
        assert!(is_esc(b"\x1b") && is_esc(b"\x1b[27u") && !is_esc(b"\x1b[A"));
        assert!(is_enter(b"\r") && !is_enter(b"a\r"));
        assert!(is_ctrl_c(b"\x03") && is_ctrl_c(b"\x1b[99;5u") && !is_ctrl_c(b"c"));
    }
}
