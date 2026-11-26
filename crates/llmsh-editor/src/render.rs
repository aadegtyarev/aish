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
    pub theme: &'a Theme,
}

#[derive(Default)]
pub(crate) struct Renderer {
    pub start_col: u16,
    pub cur_row: u16,
    cur_hard: u16,
    cur_cells: u32,
    /// End column of the drawn input when it (and nothing else) fits on the
    /// first row; None when it wraps or popups are shown.
    pub first_row_end: Option<u16>,
}

struct Lay {
    row: u16,
    col: u16,
    /// Hard newlines so far, and cells since the last one (for resize math).
    hard: u16,
    cells: u32,
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
            lay.row += 1;
            lay.col = 0;
            lay.hard += 1;
            lay.cells = 0;
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
        lay.row += 1;
        lay.col = 0;
    }
    if let Some(r) = rec {
        *r = (lay.row, lay.col);
    }
    out.extend_from_slice(bytes);
    lay.col += w;
    lay.cells += u32::from(w);
    if lay.col >= width {
        out.extend_from_slice(b"\r\n");
        lay.row += 1;
        lay.col = 0;
    }
}

impl Renderer {
    pub(crate) fn reset(&mut self, start_col: u16) {
        self.start_col = start_col;
        self.cur_row = 0;
        self.cur_hard = 0;
        self.cur_cells = u32::from(start_col);
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
        self.cur_hard = 0;
        self.cur_cells = u32::from(self.start_col);
    }

    pub(crate) fn draw(&mut self, out: &mut Vec<u8>, v: &View, width: u16) {
        let width = width.max(4);
        self.start_col = self.start_col.min(width - 1);
        out.extend_from_slice(b"\x1b[?2026h");
        self.to_origin(out);
        let mut lay = Lay {
            row: 0,
            col: self.start_col,
            hard: 0,
            cells: u32::from(self.start_col),
        };
        let mut cur = (0u16, self.start_col);
        let mut cur_hc = (0u16, u32::from(self.start_col));
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
                cur_hc = (lay.hard, lay.cells);
            }
            emit(out, &mut lay, g, width, rec);
        }
        if style != Class::Default {
            out.extend_from_slice(b"\x1b[0m");
        }
        if !cur_set {
            cur = (lay.row, lay.col);
            cur_hc = (lay.hard, lay.cells);
        }
        if v.theme.enabled && !v.ghost.is_empty() {
            let _ = write!(out, "\x1b[0;{}m", v.theme.ghost);
            let mut gl = Lay {
                row: lay.row,
                col: lay.col,
                hard: lay.hard,
                cells: lay.cells,
            };
            for g in v.ghost.graphemes(true) {
                emit(out, &mut gl, g, width, None);
            }
            out.extend_from_slice(b"\x1b[0m");
            lay = gl;
        }
        out.extend_from_slice(b"\x1b[J");
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
        self.cur_hard = cur_hc.0;
        self.cur_cells = cur_hc.1;
        self.first_row_end = (lay.row == 0 && v.extra_rows == 0).then_some(lay.col);
    }

    /// Erase after the terminal was resized to `width` columns: assume the
    /// terminal reflowed the wrapped text, so the cursor row is computed with
    /// the new width instead of the one we drew with.
    pub(crate) fn erase_resized(&mut self, out: &mut Vec<u8>, width: u16) {
        let w = u32::from(width.max(1));
        let row = u32::from(self.cur_hard) + self.cur_cells / w;
        out.extend_from_slice(b"\x1b[?2026h");
        if row > 0 {
            let _ = write!(out, "\x1b[{row}A");
        }
        let _ = write!(out, "\x1b[{}G\x1b[J\x1b[?2026l", self.start_col + 1);
        self.cur_row = 0;
        self.cur_hard = 0;
        self.cur_cells = u32::from(self.start_col);
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
    fn control_chars_are_carets() {
        let mut r = Renderer::default();
        let mut out = vec![];
        r.draw(&mut out, &view("a\x01", 2, ""), 80);
        assert!(String::from_utf8(out).unwrap().contains("^A"));
    }
}
