//! Text + cursor with emacs-style edits, kill ring (one slot) and undo.
//! Cursor is a byte offset on a grapheme boundary. Steady-state edits do not
//! allocate: undo snapshots reuse their strings.

use unicode_segmentation::UnicodeSegmentation;

const UNDO_MAX: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    None,
    Insert,
    Delete,
    Other,
}

#[derive(Default)]
pub(crate) struct Buffer {
    text: String,
    cursor: usize,
    undo: Vec<(String, usize)>,
    undo_len: usize,
    last: Option<Kind>,
    kill: String,
    rev: u64,
}

/// Largest char boundary of `s` that is <= `i` (and <= len).
pub(crate) fn floor_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Buffer {
    pub(crate) fn new() -> Self {
        Buffer {
            text: String::with_capacity(256),
            ..Default::default()
        }
    }

    /// Incremented on every content change.
    pub(crate) fn rev(&self) -> u64 {
        self.rev
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }
    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
    pub(crate) fn at_end(&self) -> bool {
        self.cursor == self.text.len()
    }

    fn checkpoint(&mut self, kind: Kind) {
        self.rev += 1;
        if kind == Kind::Insert && self.last == Some(Kind::Insert) {
            return;
        }
        self.last = Some(kind);
        if self.undo_len == UNDO_MAX {
            self.undo.rotate_left(1);
            self.undo_len -= 1;
        }
        let slot = self.undo_len;
        if slot < self.undo.len() {
            let (s, c) = &mut self.undo[slot];
            s.clear();
            s.push_str(&self.text);
            *c = self.cursor;
        } else {
            self.undo.push((self.text.clone(), self.cursor));
        }
        self.undo_len += 1;
    }

    /// Navigation breaks the typing run so the next insert starts a new undo step.
    fn nav(&mut self) {
        self.rev += 1; // cursor moves invalidate late results computed for the old position
        self.last = Some(Kind::None);
    }

    /// Replace everything (history recall, completion preview): undoable.
    pub(crate) fn set(&mut self, s: &str) {
        self.checkpoint(Kind::Other);
        self.text.clear();
        self.text.push_str(s);
        self.cursor = self.text.len();
    }

    /// Drop all content and history of edits.
    pub(crate) fn clear(&mut self) {
        self.rev += 1;
        self.text.clear();
        self.cursor = 0;
        self.undo_len = 0;
        self.last = None;
    }

    pub(crate) fn take(&mut self) -> String {
        let s = self.text.clone();
        self.clear();
        s
    }

    pub(crate) fn set_cursor(&mut self, c: usize) {
        self.cursor = floor_boundary(&self.text, c);
        self.nav();
    }

    pub(crate) fn insert(&mut self, c: char) {
        self.checkpoint(Kind::Insert);
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub(crate) fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.checkpoint(Kind::Other);
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor]
            .graphemes(true)
            .next_back()
            .map_or(0, |g| self.cursor - g.len())
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len())
    }

    pub(crate) fn left(&mut self) {
        self.cursor = self.prev_boundary();
        self.nav();
    }
    pub(crate) fn right(&mut self) {
        self.cursor = self.next_boundary();
        self.nav();
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }
    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i)
    }
    pub(crate) fn home(&mut self) {
        self.cursor = self.line_start();
        self.nav();
    }
    pub(crate) fn end(&mut self) {
        self.cursor = self.line_end();
        self.nav();
    }

    fn word_left_pos(&self) -> usize {
        let mut it = self.text[..self.cursor].char_indices().rev().peekable();
        while it.next_if(|&(_, c)| !is_word(c)).is_some() {}
        let mut pos = None;
        while let Some((i, _)) = it.next_if(|&(_, c)| is_word(c)) {
            pos = Some(i);
        }
        pos.unwrap_or_else(|| it.peek().map_or(0, |&(i, c)| i + c.len_utf8()))
    }

    pub(crate) fn word_right_pos_in(s: &str) -> usize {
        let mut it = s.char_indices().peekable();
        while it.next_if(|&(_, c)| !is_word(c)).is_some() {}
        while it.next_if(|&(_, c)| is_word(c)).is_some() {}
        it.peek().map_or(s.len(), |&(i, _)| i)
    }

    fn word_right_pos(&self) -> usize {
        self.cursor + Self::word_right_pos_in(&self.text[self.cursor..])
    }

    fn ws_word_left_pos(&self) -> usize {
        let mut it = self.text[..self.cursor].char_indices().rev().peekable();
        while it.next_if(|&(_, c)| c.is_whitespace()).is_some() {}
        let mut pos = None;
        while let Some((i, _)) = it.next_if(|&(_, c)| !c.is_whitespace()) {
            pos = Some(i);
        }
        pos.unwrap_or_else(|| it.peek().map_or(0, |&(i, c)| i + c.len_utf8()))
    }

    pub(crate) fn word_left(&mut self) {
        self.cursor = self.word_left_pos();
        self.nav();
    }
    pub(crate) fn word_right(&mut self) {
        self.cursor = self.word_right_pos();
        self.nav();
    }

    fn cut(&mut self, a: usize, b: usize, kill: bool) {
        if a >= b {
            return;
        }
        self.checkpoint(Kind::Delete);
        if kill {
            self.kill.clear();
            self.kill.push_str(&self.text[a..b]);
        }
        self.text.replace_range(a..b, "");
        self.cursor = a;
    }

    pub(crate) fn backspace(&mut self) {
        let p = self.prev_boundary();
        self.cut(p, self.cursor, false);
    }
    pub(crate) fn delete(&mut self) {
        let n = self.next_boundary();
        self.cut(self.cursor, n, false);
    }
    /// Ctrl-W: back to the previous whitespace-delimited word start.
    pub(crate) fn kill_ws_word_back(&mut self) {
        let p = self.ws_word_left_pos();
        self.cut(p, self.cursor, true);
    }
    /// Alt-Backspace.
    pub(crate) fn kill_word_back(&mut self) {
        let p = self.word_left_pos();
        self.cut(p, self.cursor, true);
    }
    /// Alt-D.
    pub(crate) fn kill_word_fwd(&mut self) {
        let p = self.word_right_pos();
        self.cut(self.cursor, p, true);
    }
    pub(crate) fn kill_to_end(&mut self) {
        let e = if self.text[self.cursor..].starts_with('\n') {
            self.cursor + 1
        } else {
            self.line_end()
        };
        self.cut(self.cursor, e, true);
    }
    pub(crate) fn kill_to_start(&mut self) {
        let s = self.line_start();
        self.cut(s, self.cursor, true);
    }

    pub(crate) fn yank(&mut self) {
        if self.kill.is_empty() {
            return;
        }
        self.checkpoint(Kind::Other);
        self.text.insert_str(self.cursor, &self.kill);
        self.cursor += self.kill.len();
    }

    pub(crate) fn transpose(&mut self) {
        let end = if self.at_end() {
            self.cursor
        } else {
            self.next_boundary()
        };
        let mid = self.text[..end]
            .graphemes(true)
            .next_back()
            .map_or(0, |g| end - g.len());
        let start = self.text[..mid]
            .graphemes(true)
            .next_back()
            .map_or(0, |g| mid - g.len());
        if start == mid || mid == end {
            return;
        }
        self.checkpoint(Kind::Other);
        let a = self.text[start..mid].to_owned();
        let b = self.text[mid..end].to_owned();
        self.text.replace_range(start..end, "");
        self.text.insert_str(start, &b);
        self.text.insert_str(start + b.len(), &a);
        self.cursor = end;
    }

    pub(crate) fn undo(&mut self) {
        if self.undo_len == 0 {
            return;
        }
        self.rev += 1;
        self.undo_len -= 1;
        let (s, c) = &self.undo[self.undo_len];
        self.text.clear();
        self.text.push_str(s);
        self.cursor = *c;
        self.last = Some(Kind::None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Buffer {
        let mut b = Buffer::new();
        b.insert_str(s);
        b
    }

    #[test]
    fn grapheme_moves() {
        let mut x = b("a中e\u{301}😀");
        x.left();
        assert_eq!(&x.text()[x.cursor()..], "😀");
        x.left();
        assert_eq!(&x.text()[x.cursor()..], "e\u{301}😀");
        x.left();
        x.left();
        assert_eq!(x.cursor(), 0);
        x.right();
        x.right();
        assert_eq!(&x.text()[x.cursor()..], "e\u{301}😀");
        x.backspace();
        assert_eq!(x.text(), "ae\u{301}😀");
    }

    #[test]
    fn word_moves() {
        let mut x = b("git  checkout-b 中文 x");
        x.word_left();
        assert_eq!(&x.text()[x.cursor()..], "x");
        x.word_left();
        assert_eq!(&x.text()[x.cursor()..], "中文 x");
        x.word_left();
        x.word_left();
        assert_eq!(&x.text()[x.cursor()..], "checkout-b 中文 x");
        x.set_cursor(0);
        x.word_right();
        assert_eq!(x.cursor(), 3);
        x.word_right();
        assert_eq!(&x.text()[..x.cursor()], "git  checkout");
    }

    #[test]
    fn kills_and_yank() {
        let mut x = b("echo foo/bar baz");
        x.kill_ws_word_back();
        assert_eq!(x.text(), "echo foo/bar ");
        x.kill_word_back();
        assert_eq!(x.text(), "echo foo/");
        x.yank();
        assert_eq!(x.text(), "echo foo/bar ");
        x.set_cursor(0);
        x.kill_word_fwd();
        assert_eq!(x.text(), " foo/bar ");
        x.kill_to_end();
        assert_eq!(x.text(), "");
        x.yank();
        assert_eq!(x.text(), " foo/bar ");
        x.set_cursor(4);
        x.kill_to_start();
        assert_eq!(x.text(), "/bar ");
    }

    #[test]
    fn transpose_cases() {
        let mut x = b("ab中");
        x.transpose();
        assert_eq!(x.text(), "a中b");
        x.set_cursor(1);
        x.transpose();
        assert_eq!(x.text(), "中ab");
        assert_eq!(x.cursor(), "中a".len());
    }

    #[test]
    fn undo_coalesces_typing() {
        let mut x = Buffer::new();
        for c in "hello".chars() {
            x.insert(c);
        }
        x.left();
        x.insert('X');
        x.undo();
        assert_eq!(x.text(), "hello");
        x.undo();
        assert_eq!(x.text(), "");
        x.undo();
        assert_eq!(x.text(), "");
        let mut y = b("ab cd");
        y.kill_word_back();
        y.undo();
        assert_eq!(y.text(), "ab cd");
    }

    #[test]
    fn multiline_home_end() {
        let mut x = b("one\ntwo");
        x.home();
        assert_eq!(&x.text()[x.cursor()..], "two");
        x.set_cursor(1);
        x.end();
        assert_eq!(x.cursor(), 3);
    }
}
