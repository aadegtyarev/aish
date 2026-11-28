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
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use aish_editor::{Editor, Event, Theme};

use crate::blocks::BlockTracker;
use crate::llm::ask::{self, Screen};
use crate::llm::render::{sanitize, Renderer};
use crate::llm::Llm;
use crate::task::{TaskMaker, TaskShared};

mod gate;
mod key;
pub use gate::*;
use gate::{enter_bytes, nudge, restore_bytes};
pub use key::*;

/// Everything the overlay needs; made under the controller lock.
pub struct Setup {
    pub key: AskKey,
    pub llm: Arc<Llm>,
    pub tracker: Arc<Mutex<BlockTracker>>,
    pub tasks: TaskMaker,
    pub theme: Theme,
    pub stdout: File,
    pub gate: Arc<Gate>,
    /// Terminal modes the program had set when the overlay opened.
    pub modes: Modes,
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
    let one: String = sanitize(cmd.unwrap_or("command"))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let meta = if alt {
        "full-screen app".to_owned()
    } else if lines == 0 {
        "no output".to_owned()
    } else {
        format!("last {lines} lines")
    };
    let max = cols.saturating_sub(1 + 2 + 3 + meta.chars().count()).max(4);
    let c = if one.chars().count() > max {
        let t: String = one.chars().take(max - 1).collect();
        format!("{t}…")
    } else {
        one
    };
    // narrow screens: never wider than the line
    crate::llm::render::fit_line(&format!("  {c} · {meta}"), cols)
}

/// The key hints, the longest variant that fits one row of `cols` columns.
pub fn footer_text(insert: bool, cols: usize) -> &'static str {
    let tiers: &[&str] = if insert {
        &[
            "  Enter — insert · Esc — close",
            "  Enter — insert · Esc",
            "  Enter · Esc",
        ]
    } else {
        &["  Esc — close", "  Esc"]
    };
    tiers
        .iter()
        .copied()
        .find(|t| t.chars().count() < cols)
        .unwrap_or(tiers[tiers.len() - 1])
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
            self.s.modes.alt,
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
        self.s.gate.set_restore(restore_bytes(self.s.modes));
        self.w(&enter_bytes(self.s.modes.alt));
        self.ed.clear();
        self.frame();
    }

    fn footer(&mut self, insert: bool) {
        let t = footer_text(insert, cols() as usize);
        let d = self.dim(t);
        self.w(format!("\r\n{d}").as_bytes());
    }

    /// The answer is complete (or failed): show what can be done next.
    fn answered(&mut self, prefill: Option<String>) {
        let insert = prefill.and_then(|c| insert_bytes(&c, self.s.modes.bracketed));
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
                if let Some(t) = self.s.llm.last_answer() {
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
                "no model configured · {} config",
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
                        // (what was typed right after it goes on to the program too)
                        return Some(b[s..].to_vec());
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
            if self.s.gate.closing.load(Ordering::SeqCst) {
                return Some(Vec::new()); // the shell is gone
            }
            let (inp, woke) = crate::pty::wait_input(stdin, wake, None);
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
    fn leave(&mut self) {
        if let St::Streaming { shared } = &self.st {
            // after this no worker text can reach the restored screen
            shared.mute();
            shared.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(r) = self.s.gate.take_restore() {
            self.w(&r);
        }
        self.s.gate.close();
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
    let _ = catch_unwind(AssertUnwindSafe(|| ov.leave()));
    ov.s.gate.close();
    crate::term::IN_GUARD.with(|x| x.set(false));
    let ok = match send {
        Some(b) if !b.is_empty() => master.write_all(&b).is_ok(),
        _ => true,
    };
    if ov.s.modes.alt && !ov.s.gate.closing.load(Ordering::SeqCst) {
        nudge(master); // the program is unblocked: it can answer with a redraw
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn footer_shortens_to_fit() {
        assert_eq!(footer_text(true, 80), "  Enter — insert · Esc — close");
        assert_eq!(footer_text(true, 30), "  Enter — insert · Esc");
        assert_eq!(footer_text(true, 20), "  Enter · Esc");
        assert_eq!(footer_text(false, 10), "  Esc");
    }

    #[test]
    fn header_line() {
        assert_eq!(
            header(Some("ssh prod"), 200, false, 80),
            "  ssh prod · last 200 lines"
        );
        assert_eq!(header(Some("cat"), 0, false, 80), "  cat · no output");
        assert_eq!(header(None, 3, true, 80), "  command · full-screen app");
        let long = header(Some(&"x".repeat(200)), 5, false, 40);
        assert!(long.chars().count() <= 40 && long.contains('…'), "{long}");
        assert!(!header(Some("a\x1b[31m\tb"), 1, false, 80).contains('\x1b'));
    }

    #[test]
    fn keys_in_the_overlay() {
        assert!(is_esc(b"\x1b") && is_esc(b"\x1b[27u") && !is_esc(b"\x1b[A"));
        assert!(is_enter(b"\r") && !is_enter(b"a\r"));
        assert!(is_ctrl_c(b"\x03") && is_ctrl_c(b"\x1b[99;5u") && !is_ctrl_c(b"c"));
    }
}
