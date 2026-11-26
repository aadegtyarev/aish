//! Standalone mode: the editor prints a prompt and reads the terminal itself.

use std::io::{self, Read, Write};

use unicode_width::UnicodeWidthChar;

use crate::editor::{Editor, Event};

/// Prompt strings. ANSI escape sequences are allowed (they count as zero width).
#[derive(Debug, Clone, Default)]
pub struct Prompt {
    /// Printed before the input; may contain newlines (only the last line
    /// shares a row with the input).
    pub left: String,
    /// Optional text at the right edge of the first row, redrawn after every
    /// edit while the line leaves room for it.
    pub right: String,
}

impl Prompt {
    /// A prompt with only a left part.
    pub fn new(left: impl Into<String>) -> Self {
        Prompt {
            left: left.into(),
            right: String::new(),
        }
    }
}

/// How a [`LineReader::read_line`] call ended.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Signal {
    /// A finished line.
    Line(String),
    /// A line claimed by an intercept plugin.
    Intercepted(String),
    /// Ctrl-C.
    Interrupt,
    /// Ctrl-D on an empty line (or end of input).
    Eof,
}

/// Reads lines from the terminal with an [`Editor`].
pub struct LineReader {
    editor: Editor,
}

struct Raw(libc::termios);

impl Raw {
    fn enter() -> Option<Raw> {
        // SAFETY: plain termios calls on fd 0 with a zeroed, then filled, struct.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            // keep output post-processing so "\n" still returns the carriage
            t.c_oflag |= libc::OPOST;
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return None;
            }
            Some(Raw(orig))
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: restores the attributes saved in `enter`.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}

fn term_width() -> u16 {
    // SAFETY: ioctl with a valid winsize out-pointer.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return ws.ws_col;
        }
    }
    80
}

fn readable(ms: i32) -> bool {
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut fd, 1, ms) > 0 }
}

/// Display width of `s` ignoring escape sequences, and of its last line.
fn visible_last_line_width(s: &str) -> u16 {
    let mut w = 0u16;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\n' => w = 0,
            '\x1b' => match it.next() {
                Some('[') => {
                    for n in it.by_ref() {
                        if ('\x40'..='\x7e').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(n) = it.next() {
                        if n == '\x07' || (n == '\x1b' && it.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            c => w = w.saturating_add(c.width().unwrap_or(0) as u16),
        }
    }
    w
}

fn crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

impl LineReader {
    /// Wrap a configured editor.
    pub fn new(editor: Editor) -> Self {
        LineReader { editor }
    }

    /// Access the editor (e.g. to inspect the theme).
    pub fn editor(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// Show `prompt`, edit a line, return how it ended. Without a terminal
    /// on stdin a plain line is read.
    pub fn read_line(&mut self, prompt: &Prompt) -> io::Result<Signal> {
        let mut out = io::stdout();
        let Some(_raw) = Raw::enter() else {
            write!(out, "{}", prompt.left)?;
            out.flush()?;
            let mut s = String::new();
            return Ok(if io::stdin().read_line(&mut s)? == 0 {
                Signal::Eof
            } else {
                Signal::Line(s.trim_end_matches(['\n', '\r']).to_owned())
            });
        };
        self.editor.clear();
        out.write_all(b"\x1b[?2004h")?;
        let r = self.run(prompt, &mut out);
        out.write_all(b"\x1b[?2004l")?;
        out.flush()?;
        r
    }

    fn print_prompt(&mut self, prompt: &Prompt, out: &mut impl Write) -> io::Result<()> {
        let width = term_width();
        out.write_all(b"\r")?;
        out.write_all(crlf(&prompt.left).as_bytes())?;
        let col = visible_last_line_width(&prompt.left);
        self.editor.reset_screen(col % width);
        Ok(())
    }

    /// Draw the input, then the right prompt if the line leaves room for it.
    fn draw(&mut self, prompt: &Prompt, out: &mut impl Write) -> io::Result<()> {
        let width = term_width();
        self.editor.draw(out, width)?;
        let rw = visible_last_line_width(&prompt.right);
        if let (true, Some(end)) = (rw > 0, self.editor.first_row_end()) {
            if end + rw + 1 < width {
                write!(out, "\x1b7\x1b[{}G{}\x1b8", width - rw + 1, prompt.right)?;
            }
        }
        out.flush()
    }

    fn run(&mut self, prompt: &Prompt, out: &mut impl Write) -> io::Result<Signal> {
        self.print_prompt(prompt, out)?;
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 4096];
        let mut events = Vec::new();
        self.draw(prompt, out)?;
        loop {
            events.clear();
            let changed;
            let wait = if self.editor.has_pending_input() {
                30
            } else {
                25
            };
            if !readable(wait) {
                if self.editor.has_pending_input() {
                    self.editor.flush_input(&mut events);
                }
                self.editor.poll(&mut events);
                changed = !events.is_empty();
            } else {
                changed = true;
                let n = stdin.read(&mut buf)?;
                if n == 0 {
                    return Ok(Signal::Eof);
                }
                self.editor.input(&buf[..n], &mut events);
            }
            for ev in events.drain(..) {
                match ev {
                    Event::Submit(l) => return self.finish(out, l, false),
                    Event::Intercepted(l) => return self.finish(out, l, true),
                    Event::PassKey(b) => match b.as_slice() {
                        [0x03] => {
                            out.write_all(b"^C\r\n")?;
                            out.flush()?;
                            return Ok(Signal::Interrupt);
                        }
                        [0x04] => {
                            out.write_all(b"\r\n")?;
                            out.flush()?;
                            return Ok(Signal::Eof);
                        }
                        [0x0c] => {
                            out.write_all(b"\x1b[H\x1b[2J")?;
                            self.print_prompt(prompt, out)?;
                        }
                        _ => {}
                    },
                    Event::CursorReport(..) | Event::NeedRedraw => {}
                }
            }
            if changed {
                self.draw(prompt, out)?;
            }
        }
    }

    fn finish(&mut self, out: &mut impl Write, l: String, intercepted: bool) -> io::Result<Signal> {
        self.editor.draw_final(out, term_width(), &l)?;
        out.write_all(b"\r\n")?;
        out.flush()?;
        Ok(if intercepted {
            Signal::Intercepted(l)
        } else {
            Signal::Line(l)
        })
    }
}
