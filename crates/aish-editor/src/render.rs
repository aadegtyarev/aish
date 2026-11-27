//! Draws the input region (text, ghost, extra rows) below the prompt that bash
//! printed. Only this region is ever touched: we remember where it starts
//! (column on the first row) and where the cursor was left (rows below start).

use std::io::Write;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::highlight::{Class, Span};
use crate::theme::Theme;

pub(crate) struct View<'a> {
    pub text: &'a str,
    pub cursor: usize,
    pub spans: &'a [Span],
    pub ghost: &'a str,
    /// Pre-formatted rows below the input, separated by `\n`.
    pub extra: &'a [u8],
    pub extra_rows: u16,
    /// Right-edge hint (see `Decoration::right_hint`).
    pub right: Option<&'a str>,
    pub theme: &'a Theme,
}

#[derive(Default)]
pub(crate) struct Renderer {
    /// Column on the first row where the input starts (at the current width).
    pub start_col: u16,
    /// Offset of the input start in its terminal line: more than the column
    /// when a resize made the terminal wrap the prompt's row.
    start_off: u32,
    pub cur_row: u16,
    /// Lengths of the drawn rows above the cursor row (the first one counts
    /// from the line start), and the cursor's offset in its own row. Every
    /// row is ended by us with CR LF, so a resize reflows each one by itself.
    row_lens: Vec<u32>,
    cur_off: u32,
    /// End column of the drawn input when it (and nothing else) fits on the
    /// first row; None when it wraps or popups are shown.
    pub first_row_end: Option<u16>,
}

struct Lay {
    row: u16,
    col: u16,
    /// Cells on the first row that lie before `col` 0 (see `Renderer::start_off`).
    base: u32,
    /// Length of every finished row (not recorded for the suggestion).
    lens: Vec<u32>,
    rec: bool,
}

impl Lay {
    fn end_row(&mut self) {
        let b = if self.row == 0 { self.base } else { 0 };
        if self.rec {
            self.lens.push(b + u32::from(self.col));
        }
        self.row += 1;
        self.col = 0;
    }
}

fn sgr(out: &mut Vec<u8>, c: Class, t: &Theme) {
    let p = match c {
        Class::Default => "",
        Class::Error => &t.error,
        Class::Str => &t.string,
        Class::Var => &t.variable,
        Class::Accent => &t.accent,
        Class::Comment => &t.comment,
    };
    let _ = write!(out, "\x1b[0;{p}m");
}

/// Emit one grapheme at `lay`, wrapping like a terminal would but without
/// ever relying on deferred-wrap state: after a full row we move on at once.
fn emit(out: &mut Vec<u8>, lay: &mut Lay, g: &str, width: u16, rec: Option<&mut (u16, u16)>) {
    let mut caret = [b'^', 0];
    let (bytes, w): (&[u8], u16) = match g {
        "\n" | "\r\n" => {
            if let Some(r) = rec {
                *r = (lay.row, lay.col);
            }
            out.extend_from_slice(b"\x1b[K\r\n");
            lay.end_row();
            return;
        }
        _ => {
            let c = g.as_bytes()[0];
            if g.len() == 1 && (c < 0x20 || c == 0x7f) {
                caret[1] = c ^ 0x40;
                (&caret, 2)
            } else {
                (g.as_bytes(), UnicodeWidthStr::width(g).min(2) as u16)
            }
        }
    };
    if w > 0 && lay.col + w > width {
        out.extend_from_slice(b"\x1b[K\r\n");
        lay.end_row();
    }
    if let Some(r) = rec {
        *r = (lay.row, lay.col);
    }
    out.extend_from_slice(bytes);
    lay.col += w;
    if lay.col >= width {
        out.extend_from_slice(b"\r\n");
        lay.end_row();
    }
}

impl Renderer {
    pub(crate) fn reset(&mut self, start_col: u16) {
        self.start_col = start_col;
        self.start_off = u32::from(start_col);
        self.cur_row = 0;
        self.row_lens.clear();
        self.cur_off = u32::from(start_col);
    }

    fn to_origin(&self, out: &mut Vec<u8>) {
        if self.cur_row > 0 {
            let _ = write!(out, "\x1b[{}A", self.cur_row);
        }
        let _ = write!(out, "\x1b[{}G", self.start_col + 1);
    }

    /// Back to the end of the prompt and clear everything below.
    pub(crate) fn erase(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b[?2026h");
        self.to_origin(out);
        out.extend_from_slice(b"\x1b[J\x1b[?2026l");
        self.cur_row = 0;
        self.row_lens.clear();
        self.cur_off = self.start_off;
    }

    pub(crate) fn draw(&mut self, out: &mut Vec<u8>, v: &View, width: u16) {
        let width = width.max(4);
        // (a prompt wider than the terminal after a resize starts mid-row)
        self.start_col = (self.start_off % u32::from(width)) as u16;
        let base = self.start_off - u32::from(self.start_col);
        out.extend_from_slice(b"\x1b[?2026h");
        self.to_origin(out);
        let mut lay = Lay {
            row: 0,
            col: self.start_col,
            base,
            lens: std::mem::take(&mut self.row_lens),
            rec: true,
        };
        lay.lens.clear();
        let mut cur = (0u16, self.start_col);
        let mut cur_lens = 0;
        let mut cur_set = false;
        let mut si = 0;
        let mut style = Class::Default;
        for (i, g) in v.text.grapheme_indices(true) {
            if v.theme.enabled {
                while si < v.spans.len() && v.spans[si].end <= i {
                    si += 1;
                }
                let cls = match v.spans.get(si) {
                    Some(s) if s.start <= i => s.class,
                    _ => Class::Default,
                };
                if cls != style {
                    sgr(out, cls, v.theme);
                    style = cls;
                }
            }
            let rec = (i == v.cursor).then_some(&mut cur);
            if rec.is_some() {
                cur_set = true;
                cur_lens = lay.lens.len();
            }
            emit(out, &mut lay, g, width, rec);
        }
        if style != Class::Default {
            out.extend_from_slice(b"\x1b[0m");
        }
        if !cur_set {
            cur = (lay.row, lay.col);
            cur_lens = lay.lens.len();
        }
        if v.theme.enabled && !v.ghost.is_empty() {
            let _ = write!(out, "\x1b[0;{}m", v.theme.ghost);
            let mut gl = Lay {
                row: lay.row,
                col: lay.col,
                base,
                lens: Vec::new(),
                rec: false,
            };
            for g in v.ghost.graphemes(true) {
                emit(out, &mut gl, g, width, None);
            }
            out.extend_from_slice(b"\x1b[0m");
            lay.row = gl.row;
            lay.col = gl.col;
        }
        out.extend_from_slice(b"\x1b[J");
        if let Some(h) = v.right.filter(|h| v.theme.enabled && !h.is_empty()) {
            let w = usize::from(width);
            let hw = UnicodeWidthStr::width(h);
            // last column stays free; keep 3 columns between input and hint
            if lay.row == 0 && hw + 1 < w && usize::from(lay.col) + 3 + hw < w {
                let col = w - 1 - hw;
                let _ = write!(out, "\x1b[{}G\x1b[0;{}m{h}\x1b[0m", col + 1, v.theme.dim);
            }
        }
        let mut end_row = lay.row;
        if v.extra_rows > 0 {
            for line in v.extra.split(|&b| b == b'\n') {
                out.extend_from_slice(b"\r\n");
                out.extend_from_slice(line);
                end_row += 1;
            }
            out.extend_from_slice(b"\x1b[0m");
        }
        if end_row > cur.0 {
            let _ = write!(out, "\x1b[{}A", end_row - cur.0);
        }
        let _ = write!(out, "\x1b[{}G", cur.1 + 1);
        out.extend_from_slice(b"\x1b[?2026l");
        self.cur_row = cur.0;
        lay.lens.truncate(cur_lens);
        self.row_lens = lay.lens;
        self.cur_off = u32::from(cur.1) + if cur.0 == 0 { base } else { 0 };
        self.first_row_end = (lay.row == 0 && v.extra_rows == 0).then_some(lay.col);
    }

    /// Erase after the terminal was resized to `width` columns. The terminal
    /// reflowed every row we drew (each ends in CR LF, so rows are not joined,
    /// only split or, for a prompt it had split before, rejoined): the rows
    /// between the input start and the cursor are counted with the new width.
    pub(crate) fn erase_resized(&mut self, out: &mut Vec<u8>, width: u16) -> bool {
        let w = u32::from(width.max(1));
        let rows = |len: u32| len.div_ceil(w).max(1);
        let cursor_row: u32 =
            self.row_lens.iter().map(|&l| rows(l)).sum::<u32>() + self.cur_off / w;
        let up = cursor_row.saturating_sub(self.start_off / w);
        out.extend_from_slice(b"\x1b[?2026h");
        if up > 0 {
            let _ = write!(out, "\x1b[{up}A");
        }
        let col = (self.start_off % w) as u16;
        let _ = write!(out, "\x1b[{}G\x1b[J", col + 1);
        // A prompt the terminal wrapped: readline repaints it from the row the
        // cursor is on (it clears that row only), so wait for it at the top.
        let top = self.start_off / w;
        if top > 0 {
            let _ = write!(out, "\x1b[{top}A\x1b[1G");
        }
        out.extend_from_slice(b"\x1b[?2026l");
        self.start_col = col;
        self.cur_row = 0;
        self.row_lens.clear();
        self.cur_off = self.start_off;
        top > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TH: std::sync::LazyLock<Theme> = std::sync::LazyLock::new(Theme::default);

    fn view<'a>(t: &'a str, c: usize, ghost: &'a str) -> View<'a> {
        View {
            text: t,
            cursor: c,
            spans: &[],
            ghost,
            extra: b"",
            extra_rows: 0,
            right: None,
            theme: &TH,
        }
    }

    #[test]
    fn simple_line_places_cursor() {
        let mut r = Renderer::default();
        r.reset(2);
        let mut out = vec![];
        r.draw(&mut out, &view("ab", 1, ""), 80);
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("ab") && s.ends_with("\x1b[4G\x1b[?2026l"),
            "{s:?}"
        );
    }

    #[test]
    fn wraps_and_tracks_rows() {
        let mut r = Renderer::default();
        r.reset(0);
        let mut out = vec![];
        // 10 columns: 25 chars -> rows 0,1,2; cursor at end is row 2 col 5.
        let t = "a".repeat(25);
        r.draw(&mut out, &view(&t, 25, ""), 10);
        assert_eq!(r.cur_row, 2);
        // Exactly full row: cursor moves to the next row start.
        out.clear();
        r.draw(&mut out, &view(&"a".repeat(20), 20, ""), 10);
        assert_eq!(r.cur_row, 2);
        // Wide char that does not fit wraps whole.
        out.clear();
        r.reset(0);
        r.draw(&mut out, &view(&format!("{}中", "a".repeat(9)), 10, ""), 10);
        assert_eq!(r.cur_row, 1);
    }

    #[test]
    fn erase_goes_up() {
        let mut r = Renderer::default();
        r.reset(3);
        let mut out = vec![];
        r.draw(&mut out, &view(&"x".repeat(30), 30, ""), 10);
        out.clear();
        r.erase(&mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b[3A\x1b[4G\x1b[J"), "{s:?}");
    }

    #[test]
    fn right_hint_only_when_there_is_room() {
        let draw = |t: &str, hint: Option<&str>, w: u16, th: &Theme| {
            let mut r = Renderer::default();
            r.reset(2);
            let mut out = vec![];
            let mut v = view(t, t.len(), "");
            v.right = hint;
            v.theme = th;
            r.draw(&mut out, &v, w);
            String::from_utf8(out).unwrap()
        };
        // 20 columns: hint at 0-based column 18 (1-based 19)
        let s = draw("ls", Some("?"), 20, &TH);
        assert!(s.contains("\x1b[19G\x1b[0;2m?\x1b[0m"), "{s:?}");
        // collides (end col 2+14=16, +3 > 18): hidden
        assert!(!draw(&"a".repeat(14), Some("?"), 20, &TH).contains("\x1b[19G"));
        // just enough room (end col 2+13=15 -> 15+3 <= 18)
        assert!(draw(&"a".repeat(13), Some("?"), 20, &TH).contains("\x1b[19G"));
        // wrapped input: hidden
        assert!(!draw(&"a".repeat(30), Some("?"), 20, &TH).contains("\x1b[19G"));
        // no color: nothing
        let plain = Theme::plain();
        assert!(!draw("ls", Some("?"), 20, &plain).contains("\x1b[19G"));
        assert!(!draw("ls", None, 20, &TH).contains("\x1b[19G"));
    }

    #[test]
    fn control_chars_are_carets() {
        let mut r = Renderer::default();
        let mut out = vec![];
        r.draw(&mut out, &view("a\x01", 2, ""), 80);
        assert!(String::from_utf8(out).unwrap().contains("^A"));
    }
}
