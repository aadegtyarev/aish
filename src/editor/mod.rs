//! Our own line editor for prompt mode: pure state (`Editor`, unit-testable)
//! plus the I/O controller (`ctl`).

pub mod buffer;
pub mod ctl;
pub mod highlight;
pub mod history;
pub mod keys;
pub mod render;
pub mod scan;

use std::io::Write;

use unicode_width::UnicodeWidthChar;

use buffer::Buffer;
use highlight::{Resolver, Span};
use history::History;
use keys::Key;
use render::View;

pub const SEARCH_ROWS: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Submit,
    /// Ctrl-C: line cleared, bash gets `^C`.
    Interrupt,
    /// Ctrl-D on an empty line.
    Eof,
    /// Ctrl-L: bash clears the screen and redraws its prompt.
    ClearScreen,
}

struct Nav {
    pos: usize,
    prefix: String,
    draft: String,
}

struct Search {
    q: Buffer,
    results: Vec<usize>,
    sel: usize,
}

enum Ui {
    Edit,
    Search(Search),
}

pub struct Editor {
    pub buf: Buffer,
    pub hist: History,
    pub resolver: Resolver,
    pub color: bool,
    nav: Option<Nav>,
    ui: Ui,
    spans: Vec<Span>,
    extra: Vec<u8>,
    extra_rows: u16,
    pos: Vec<usize>,
}

impl Editor {
    pub fn new(color: bool) -> Self {
        Editor {
            buf: Buffer::new(),
            hist: History::default(),
            resolver: Resolver::default(),
            color,
            nav: None,
            ui: Ui::Edit,
            spans: Vec::with_capacity(32),
            extra: Vec::with_capacity(1024),
            extra_rows: 0,
            pos: Vec::new(),
        }
    }

    /// Ghost text: history continuation of the buffer, cursor at its end.
    pub fn ghost(&self) -> &str {
        if !self.color || !matches!(self.ui, Ui::Edit) || !self.buf.at_end() {
            return "";
        }
        let t = self.buf.text();
        self.hist.suggest(t).map_or("", |e| &e[t.len()..])
    }

    /// Recompute spans and extra rows for the next draw.
    pub fn prepare(&mut self, width: u16) {
        self.extra.clear();
        self.extra_rows = 0;
        match &self.ui {
            Ui::Edit => {
                if self.color {
                    highlight::highlight(self.buf.text(), &self.resolver, &mut self.spans);
                } else {
                    self.spans.clear();
                }
            }
            Ui::Search(s) => {
                self.spans.clear();
                let q = s.q.text();
                let ci = !q.bytes().any(|b| b.is_ascii_uppercase());
                for (row, &i) in s.results.iter().enumerate() {
                    if row > 0 {
                        self.extra.push(b'\n');
                    }
                    let sel = row == s.sel;
                    let cand = self.hist.get(i);
                    history::fuzzy(q.as_bytes(), cand.as_bytes(), ci, Some(&mut self.pos));
                    let _ = self.extra.write_all(if sel { b"\x1b[7m> " } else { b"  " });
                    let mut budget = usize::from(width).saturating_sub(4);
                    let mut bold = false;
                    let mut pi = 0;
                    for (bi, c) in cand.char_indices() {
                        let w = if c < ' ' { 1 } else { c.width().unwrap_or(0) };
                        if w > budget {
                            break;
                        }
                        budget -= w;
                        while pi < self.pos.len() && self.pos[pi] < bi {
                            pi += 1;
                        }
                        let m = pi < self.pos.len() && self.pos[pi] < bi + c.len_utf8();
                        if m != bold {
                            self.extra
                                .extend_from_slice(if m { b"\x1b[1m" } else { b"\x1b[22m" });
                            bold = m;
                        }
                        let mut tmp = [0u8; 4];
                        let c = if c < ' ' { ' ' } else { c };
                        self.extra
                            .extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                    }
                    self.extra.extend_from_slice(b"\x1b[0m\x1b[K");
                    self.extra_rows += 1;
                }
            }
        }
    }

    pub fn view(&self) -> View<'_> {
        let (text, cursor) = match &self.ui {
            Ui::Edit => (self.buf.text(), self.buf.cursor()),
            Ui::Search(s) => (s.q.text(), s.q.cursor()),
        };
        View {
            text,
            cursor,
            spans: &self.spans,
            ghost: self.ghost(),
            extra: &self.extra,
            extra_rows: self.extra_rows,
            color: self.color,
        }
    }

    /// Take the submitted line and remember it.
    pub fn take_line(&mut self) -> String {
        let line = self.buf.take();
        self.hist.push(&line);
        self.nav = None;
        self.ui = Ui::Edit;
        line
    }

    pub fn clear_line(&mut self) {
        self.buf.clear();
        self.nav = None;
        self.ui = Ui::Edit;
    }

    fn start_search(&mut self) {
        let mut s = Search {
            q: Buffer::new(),
            results: Vec::with_capacity(SEARCH_ROWS),
            sel: 0,
        };
        self.hist.search("", SEARCH_ROWS, &mut s.results);
        self.ui = Ui::Search(s);
    }

    fn search_key(&mut self, key: Key) -> Action {
        let Ui::Search(s) = &mut self.ui else {
            return Action::None;
        };
        let mut requery = false;
        match key {
            Key::Char(c) => {
                s.q.insert(c);
                requery = true;
            }
            Key::Paste(t) => {
                s.q.insert_str(t.lines().next().unwrap_or(""));
                requery = true;
            }
            Key::Backspace => {
                s.q.backspace();
                requery = true;
            }
            Key::Ctrl(b'u') => {
                s.q.kill_to_start();
                requery = true;
            }
            Key::Ctrl(b'w') => {
                s.q.kill_ws_word_back();
                requery = true;
            }
            Key::Left => s.q.left(),
            Key::Ctrl(b'r') | Key::Down | Key::Ctrl(b'n') => {
                if s.sel + 1 < s.results.len() {
                    s.sel += 1;
                }
            }
            Key::Up | Key::Ctrl(b'p') | Key::Ctrl(b's') => s.sel = s.sel.saturating_sub(1),
            Key::Enter | Key::Tab | Key::Right | Key::End => {
                let pick = s.results.get(s.sel).copied();
                self.ui = Ui::Edit;
                if let Some(i) = pick {
                    self.buf.set(self.hist.get(i));
                }
            }
            Key::Esc | Key::Ctrl(b'g') | Key::Ctrl(b'c') => self.ui = Ui::Edit,
            _ => {}
        }
        if requery {
            if let Ui::Search(s) = &mut self.ui {
                self.hist.search(s.q.text(), SEARCH_ROWS, &mut s.results);
                s.sel = 0;
            }
        }
        Action::None
    }

    fn hist_up(&mut self) {
        let mut nav = self.nav.take().unwrap_or_else(|| Nav {
            pos: self.hist.len(),
            prefix: self.buf.text().to_owned(),
            draft: self.buf.text().to_owned(),
        });
        if let Some(i) = self.hist.prev_matching(nav.pos, &nav.prefix, &nav.draft) {
            nav.pos = i;
            self.buf.set(self.hist.get(i));
        }
        self.nav = Some(nav);
    }

    fn hist_down(&mut self) {
        let Some(mut nav) = self.nav.take() else {
            return;
        };
        match self.hist.next_matching(nav.pos, &nav.prefix, &nav.draft) {
            Some(i) => {
                nav.pos = i;
                self.buf.set(self.hist.get(i));
                self.nav = Some(nav);
            }
            None => self.buf.set(&nav.draft),
        }
    }

    fn accept_ghost(&mut self, word: bool) -> bool {
        let g = self.ghost();
        if g.is_empty() {
            return false;
        }
        let n = if word {
            Buffer::word_right_pos_in(g).max(1)
        } else {
            g.len()
        };
        let part = g[..n].to_owned();
        self.buf.insert_str(&part);
        true
    }

    pub fn handle_key(&mut self, key: Key) -> Action {
        if matches!(self.ui, Ui::Search(_)) {
            return self.search_key(key);
        }
        if !matches!(key, Key::Up | Key::Down | Key::Ctrl(b'p') | Key::Ctrl(b'n')) {
            self.nav = None;
        }
        match key {
            Key::Char(c) => self.buf.insert(c),
            Key::Paste(t) => {
                let t = t.replace("\r\n", "\n").replace('\r', "\n");
                let t: String = t.chars().filter(|&c| c != '\x1b').collect();
                self.buf.insert_str(&t);
            }
            Key::Enter => return Action::Submit,
            Key::Left | Key::Ctrl(b'b') => self.buf.left(),
            Key::Right | Key::Ctrl(b'f') => {
                if !self.accept_ghost(false) {
                    self.buf.right();
                }
            }
            Key::End | Key::Ctrl(b'e') => {
                if !self.accept_ghost(false) {
                    self.buf.end();
                }
            }
            Key::AltRight | Key::CtrlRight | Key::Alt('f') => {
                if !self.accept_ghost(true) {
                    self.buf.word_right();
                }
            }
            Key::AltLeft | Key::CtrlLeft | Key::Alt('b') => self.buf.word_left(),
            Key::Home | Key::Ctrl(b'a') => self.buf.home(),
            Key::Backspace => self.buf.backspace(),
            Key::Delete => self.buf.delete(),
            Key::Ctrl(b'd') => {
                if self.buf.is_empty() {
                    return Action::Eof;
                }
                self.buf.delete();
            }
            Key::Ctrl(b'w') => self.buf.kill_ws_word_back(),
            Key::AltBackspace => self.buf.kill_word_back(),
            Key::Alt('d') => self.buf.kill_word_fwd(),
            Key::Ctrl(b'u') => self.buf.kill_to_start(),
            Key::Ctrl(b'k') => self.buf.kill_to_end(),
            Key::Ctrl(b'y') => self.buf.yank(),
            Key::Ctrl(b'_') => self.buf.undo(),
            Key::Ctrl(b't') => self.buf.transpose(),
            Key::Up | Key::Ctrl(b'p') => self.hist_up(),
            Key::Down | Key::Ctrl(b'n') => self.hist_down(),
            Key::Ctrl(b'c') => {
                self.clear_line();
                return Action::Interrupt;
            }
            Key::Ctrl(b'l') => return Action::ClearScreen,
            Key::Ctrl(b'r') => self.start_search(),
            _ => {}
        }
        Action::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed(hist: &str) -> Editor {
        let mut e = Editor::new(true);
        e.hist = History::parse(hist);
        e
    }

    fn type_str(e: &mut Editor, s: &str) {
        for c in s.chars() {
            e.handle_key(Key::Char(c));
        }
    }

    #[test]
    fn ghost_accept_all_and_word() {
        let mut e = ed("echo hello big world\n");
        type_str(&mut e, "ech");
        assert_eq!(e.ghost(), "o hello big world");
        e.handle_key(Key::AltRight);
        assert_eq!(e.buf.text(), "echo");
        assert_eq!(e.ghost(), " hello big world");
        e.handle_key(Key::Right);
        assert_eq!(e.buf.text(), "echo hello big world");
        assert_eq!(e.ghost(), "");
    }

    #[test]
    fn no_ghost_mid_line_or_no_color() {
        let mut e = ed("echo hi\n");
        type_str(&mut e, "ec");
        e.handle_key(Key::Left);
        assert_eq!(e.ghost(), "");
        let mut n = Editor::new(false);
        n.hist = History::parse("echo hi\n");
        type_str(&mut n, "ec");
        assert_eq!(n.ghost(), "");
    }

    #[test]
    fn history_prefix_nav_and_restore() {
        let mut e = ed("git a\nls\ngit b\n");
        type_str(&mut e, "git");
        e.handle_key(Key::Up);
        assert_eq!(e.buf.text(), "git b");
        e.handle_key(Key::Up);
        assert_eq!(e.buf.text(), "git a");
        e.handle_key(Key::Up);
        assert_eq!(e.buf.text(), "git a");
        e.handle_key(Key::Down);
        assert_eq!(e.buf.text(), "git b");
        e.handle_key(Key::Down);
        assert_eq!(e.buf.text(), "git");
        let mut f = ed("a\nb\n");
        f.handle_key(Key::Ctrl(b'p'));
        assert_eq!(f.buf.text(), "b");
    }

    #[test]
    fn ctrl_d_c_l_actions() {
        let mut e = ed("");
        assert_eq!(e.handle_key(Key::Ctrl(b'd')), Action::Eof);
        type_str(&mut e, "ab");
        e.handle_key(Key::Left);
        assert_eq!(e.handle_key(Key::Ctrl(b'd')), Action::None);
        assert_eq!(e.buf.text(), "a");
        assert_eq!(e.handle_key(Key::Ctrl(b'c')), Action::Interrupt);
        assert!(e.buf.is_empty());
        assert_eq!(e.handle_key(Key::Ctrl(b'l')), Action::ClearScreen);
    }

    #[test]
    fn ctrl_r_search_accepts_without_submitting() {
        let mut e = ed("cargo build\ngit commit\nls -la\n");
        type_str(&mut e, "x");
        e.handle_key(Key::Ctrl(b'r'));
        type_str(&mut e, "gc");
        e.prepare(80);
        assert_eq!(e.view().text, "gc");
        assert!(e.view().extra_rows >= 1);
        assert_eq!(e.handle_key(Key::Enter), Action::None);
        assert_eq!(e.buf.text(), "git commit");
        // cancel restores
        e.handle_key(Key::Ctrl(b'r'));
        type_str(&mut e, "ls");
        e.handle_key(Key::Esc);
        assert_eq!(e.buf.text(), "git commit");
    }

    #[test]
    fn paste_inserts_verbatim() {
        let mut e = ed("");
        e.handle_key(Key::Paste("a\r\nb\x1b".into()));
        assert_eq!(e.buf.text(), "a\nb");
    }

    #[test]
    fn key_to_render_cost() {
        let mut data = String::new();
        for i in 0..100_000 {
            data.push_str(&format!("some-command --opt {i} /path/to/file{}\n", i % 89));
        }
        let mut e = Editor::new(true);
        e.hist = History::parse(&data);
        e.resolver
            .set_path("x".into(), ["git".into()].into_iter().collect());
        let mut r = render::Renderer::default();
        r.reset(4);
        let mut out = Vec::with_capacity(4096);
        let line = "git commit -m \"fix the frobnicator\" --amend $HOME # zz";
        let mut worst = std::time::Duration::ZERO;
        let t = std::time::Instant::now();
        let mut keys = 0;
        for c in line.chars() {
            let k = std::time::Instant::now();
            e.handle_key(Key::Char(c));
            e.prepare(80);
            out.clear();
            r.draw(&mut out, &e.view(), 80);
            worst = worst.max(k.elapsed());
            keys += 1;
        }
        eprintln!(
            "key->render (no tty write), 100k history, no ghost hit: avg {:?} worst {:?}",
            t.elapsed() / keys,
            worst
        );
        assert!(worst < std::time::Duration::from_millis(5));
    }
}
