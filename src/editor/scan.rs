//! Light scanner over the terminal-bound output: the column the cursor is in
//! and the OSC 133 marks that drive prompt mode.

use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, PartialEq, Eq)]
enum St {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    /// DCS/APC/PM/SOS: swallow until ST.
    Str,
    StrEsc,
}

/// Mouse reporting modes the overlay turns off (and puts back).
pub const MOUSE_MODES: [u16; 5] = [1000, 1002, 1003, 1006, 1015];

pub struct Scan {
    pub col: u16,
    /// Rows the cursor went down since the last prompt mark (a prompt wider
    /// than the terminal, or one with newlines, spans several rows).
    pub rows: u16,
    /// How many of those rows the terminal wrapped by itself (no line feed):
    /// they belong to one terminal line, which a resize reflows together.
    pub soft_rows: u16,
    /// Column and soft rows when 133;B arrived (the end of the prompt), and
    /// whether the cursor was moved after it (readline puts it back with
    /// backspaces after repainting on SIGWINCH, leaving it inside the prompt).
    b_col: u16,
    b_soft: u16,
    pub moved_after_b: bool,
    /// A line feed just arrived; a CR right after it is readline's own wrap
    /// break (the row count goes on), anything else is a newline of the prompt
    /// string (readline repaints only the text after it).
    lf_rows: Option<u16>,
    /// A space right after 133;B: readline's trick for a prompt that fills the
    /// row (`" \r"` moves the cursor to the next row). A CR next cancels it.
    space_after_b: bool,
    st: St,
    osc: [u8; 64],
    osc_n: usize,
    csi: [u8; 24],
    csi_n: usize,
    /// Our session nonce; OSC 133 marks without it (foreign shells) are ignored.
    nonce: String,
    utf: [u8; 4],
    utf_n: usize,
    utf_need: usize,
    /// Prompt printed (133;B) and no command started since.
    pub b_seen: bool,
    /// 133;A seen since last cleared (a prompt is being (re)drawn).
    pub saw_a: bool,
    /// 133;C or D seen since last cleared.
    pub saw_cd: bool,
    /// 133;B seen since last cleared.
    pub saw_b: bool,
    /// Something printable was written after the last B: bash's line is not
    /// empty (type-ahead echoed by readline) or it is still drawing.
    pub printed_after_b: bool,
    /// The running program enabled bracketed paste (`CSI ? 2004 h`).
    pub bracketed: bool,
    /// The program is in the alternate screen (`CSI ? 1049 / 1047 / 47 h`).
    pub alt: bool,
    /// Bit i: `MOUSE_MODES[i]` is on.
    pub mouse: u8,
    /// The program hid the cursor (`CSI ? 25 l`).
    pub cursor_hidden: bool,
}

impl Default for Scan {
    fn default() -> Self {
        Scan::new(String::new())
    }
}

impl Scan {
    pub fn new(nonce: String) -> Self {
        Scan {
            nonce,
            col: 0,
            rows: 0,
            soft_rows: 0,
            b_col: 0,
            b_soft: 0,
            moved_after_b: false,
            lf_rows: None,
            space_after_b: false,
            st: St::Ground,
            osc: [0; 64],
            osc_n: 0,
            csi: [0; 24],
            csi_n: 0,
            utf: [0; 4],
            utf_n: 0,
            utf_need: 0,
            b_seen: false,
            saw_a: false,
            saw_cd: false,
            saw_b: false,
            printed_after_b: false,
            bracketed: false,
            alt: false,
            mouse: 0,
            cursor_hidden: false,
        }
    }

    pub fn feed(&mut self, data: &[u8], width: u16) {
        let width = width.max(1);
        for &b in data {
            match self.st {
                St::Ground => self.ground(b, width),
                St::Esc => {
                    self.st = match b {
                        b'[' => {
                            self.csi_n = 0;
                            St::Csi
                        }
                        b']' => {
                            self.osc_n = 0;
                            St::Osc
                        }
                        b'P' | b'_' | b'^' | b'X' => St::Str,
                        0x1b => St::Esc,
                        // Charset selection etc. take one more byte; harmless to
                        // treat it as ground text of width 0 below.
                        b'(' | b')' | b'*' | b'+' | b'#' | b'%' => St::Csi,
                        _ => St::Ground,
                    }
                }
                St::Csi => {
                    if (0x40..=0x7e).contains(&b) || b == 0x18 || b == 0x1a {
                        if matches!(b, b'h' | b'l') && self.csi[..self.csi_n].first() == Some(&b'?')
                        {
                            self.private_mode(b == b'h');
                        }
                        self.st = St::Ground;
                    } else if b == 0x1b {
                        self.st = St::Esc;
                    } else if self.csi_n < self.csi.len() {
                        self.csi[self.csi_n] = b;
                        self.csi_n += 1;
                    }
                }
                St::Osc => match b {
                    0x07 => self.end_osc(),
                    0x1b => self.st = St::OscEsc,
                    0x18 | 0x1a => self.st = St::Ground,
                    _ => {
                        if self.osc_n < self.osc.len() {
                            self.osc[self.osc_n] = b;
                            self.osc_n += 1;
                        }
                    }
                },
                St::OscEsc => {
                    if b == b'\\' {
                        self.end_osc();
                    } else {
                        self.st = St::Esc;
                        self.feed(&[b], width);
                    }
                }
                St::Str => match b {
                    0x07 | 0x18 | 0x1a => self.st = St::Ground,
                    0x1b => self.st = St::StrEsc,
                    _ => {}
                },
                St::StrEsc => {
                    self.st = if b == b'\\' { St::Ground } else { St::Str };
                }
            }
        }
    }

    /// `CSI ? Pm h|l`: remember the modes the overlay needs to know about.
    fn private_mode(&mut self, on: bool) {
        for p in self.csi[1..self.csi_n].split(|&c| c == b';') {
            match p {
                b"2004" => self.bracketed = on,
                b"1049" | b"1047" | b"47" => self.alt = on,
                b"25" => self.cursor_hidden = !on,
                m => {
                    let n = std::str::from_utf8(m)
                        .ok()
                        .and_then(|x| x.parse::<u16>().ok());
                    if let Some(i) = MOUSE_MODES.iter().position(|&x| Some(x) == n) {
                        if on {
                            self.mouse |= 1 << i;
                        } else {
                            self.mouse &= !(1 << i);
                        }
                    }
                }
            }
        }
    }

    fn end_osc(&mut self) {
        self.st = St::Ground;
        let o = &self.osc[..self.osc_n];
        let tag = format!(";aish={}", self.nonce);
        let ours = self.nonce.is_empty() || o.ends_with(tag.as_bytes());
        if ours && o.len() >= 5 && &o[..4] == b"133;" && (o.len() == 5 || o[5] == b';') {
            match o[4] {
                b'A' => {
                    self.rows = 0;
                    self.soft_rows = 0;
                    self.b_seen = false;
                    self.saw_a = true;
                }
                b'B' => {
                    self.b_col = self.col;
                    self.b_soft = self.soft_rows;
                    self.moved_after_b = false;
                    self.b_seen = true;
                    self.saw_b = true;
                    self.printed_after_b = false;
                }
                b'C' | b'D' => {
                    self.b_seen = false;
                    self.saw_cd = true;
                    self.alt = false; // a new command starts on the main screen
                }
                _ => {}
            }
        }
    }

    fn adv(&mut self, w: u16, width: u16) {
        if w == 0 {
            return;
        }
        if self.b_seen {
            self.printed_after_b = true;
        }
        if self.col + w > width {
            self.rows = self.rows.saturating_add(1);
            self.soft_rows = self.soft_rows.saturating_add(1);
            self.col = w;
        } else {
            self.col += w;
        }
    }

    fn ground(&mut self, b: u8, width: u16) {
        if std::mem::take(&mut self.space_after_b) && b != b'\r' {
            self.printed_after_b = true; // it was real typed-ahead text
        }
        if let Some(r) = self.lf_rows.take() {
            if b == b'\r' {
                self.rows = r;
            }
        }
        if self.utf_need > 0 {
            if b & 0xc0 == 0x80 {
                self.utf[self.utf_n] = b;
                self.utf_n += 1;
                if self.utf_n == self.utf_need {
                    self.utf_need = 0;
                    if let Some(c) = std::str::from_utf8(&self.utf[..self.utf_n])
                        .ok()
                        .and_then(|s| s.chars().next())
                    {
                        self.adv(c.width().unwrap_or(0) as u16, width);
                    }
                }
                return;
            }
            self.utf_need = 0; // broken sequence: reprocess b
        }
        match b {
            0x1b => self.st = St::Esc,
            b'\r' => self.col = 0,
            b'\n' | 0x0b | 0x0c => {
                self.lf_rows = Some(self.rows.saturating_add(1));
                self.rows = 0;
                self.soft_rows = 0;
            }
            0x08 => {
                self.moved_after_b |= self.b_seen;
                self.col = self.col.saturating_sub(1);
            }
            b'\t' => self.col = ((self.col / 8 + 1) * 8).min(width - 1),
            0x00..=0x1f | 0x7f => {}
            b' ' if self.b_seen && !self.printed_after_b => {
                self.space_after_b = true;
                self.adv(1, width);
                self.printed_after_b = false;
            }
            0x20..=0x7e => self.adv(1, width),
            0xc2..=0xdf => self.start_utf(b, 2),
            0xe0..=0xef => self.start_utf(b, 3),
            0xf0..=0xf4 => self.start_utf(b, 4),
            _ => {}
        }
    }

    fn start_utf(&mut self, b: u8, need: usize) {
        self.utf[0] = b;
        self.utf_n = 1;
        self.utf_need = need;
    }

    /// Where the input starts: column and soft-wrapped rows above it. The end
    /// of the prompt, not the cursor, when something moved the cursor back.
    pub fn input_start(&self, width: u16) -> (u16, u16) {
        if self.b_seen && self.moved_after_b {
            (
                if self.b_col >= width { 0 } else { self.b_col },
                self.b_soft,
            )
        } else {
            (self.column(width), self.soft_rows)
        }
    }

    /// Column on the current row after the last output (pending wrap = 0).
    pub fn column(&self, width: u16) -> u16 {
        if self.col >= width {
            0
        } else {
            self.col
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_and_marks() {
        let mut s = Scan::default();
        s.feed(b"\x1b]133;A\x07\x1b[1;32muser\x1b[0m> \x1b]133;B\x1b\\", 80);
        assert_eq!(s.col, 6);
        assert!(s.b_seen && s.saw_a);
        s.feed("日本\r\nxy".as_bytes(), 80);
        assert_eq!(s.col, 2);
        s.feed(b"\x1b]133;C\x07", 80);
        assert!(!s.b_seen && s.saw_cd);
    }

    #[test]
    fn readline_wrap_trick_after_b_is_not_typed_text() {
        let mut s = Scan::default();
        s.feed(b"\x1b]133;A\x07abc \x1b]133;B\x07 \r", 4);
        assert!(!s.printed_after_b);
        assert_eq!((s.rows, s.col), (1, 0));
        s.feed(b"x", 4); // a real key echoed
        assert!(s.printed_after_b);
    }

    #[test]
    fn prompt_rows_count_newlines_and_wraps() {
        let mut s = Scan::default();
        s.feed(b"\x1b]133;A\x07", 10);
        s.feed(b"0123456789ab", 10);
        assert_eq!((s.rows, s.col), (1, 2));
        s.feed(b"\r\n\rxy", 10); // readline's wrap break: LF then CR
        assert_eq!((s.rows, s.col), (2, 2));
        s.feed(b"\r\n$ ", 10); // a newline in PS1: only the last line counts
        assert_eq!((s.rows, s.col), (0, 2));
        s.feed(b"\x1b]133;A\x07", 10);
        assert_eq!(s.rows, 0);
    }

    #[test]
    fn split_utf8_and_osc() {
        let mut s = Scan::default();
        for chunk in [&b"\xe6"[..], b"\x97", b"\xa5\x1b]13", b"3;B\x07"] {
            s.feed(chunk, 80);
        }
        assert_eq!(s.col, 2);
        assert!(s.b_seen);
    }

    #[test]
    fn foreign_marks_are_ignored() {
        let mut s = Scan::new("abc".into());
        s.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07", 80); // untagged: ssh/docker/nested shell
        assert!(!s.b_seen && !s.saw_a && !s.saw_b);
        s.feed(b"\x1b]133;B;aish=zzz\x07", 80);
        assert!(!s.b_seen);
        s.feed(b"\x1b]133;A;aish=abc\x07$ \x1b]133;B;aish=abc\x07", 80);
        assert!(s.b_seen && s.saw_a);
        s.feed(b"\x1b]133;D;130;aish=abc\x07", 80);
        assert!(!s.b_seen && s.saw_cd);
    }

    #[test]
    fn echo_after_prompt_marks_line_not_empty() {
        let mut s = Scan::default();
        s.feed(b"\x1b]133;B\x07\x1b[K\x1b[?2004h", 80);
        assert!(!s.printed_after_b && s.bracketed);
        s.feed(b"ls", 80); // readline echoing type-ahead
        assert!(s.printed_after_b);
        s.feed(b"\x1b]133;B\x07", 80);
        assert!(!s.printed_after_b);
        s.feed(b"\x1b[?2004l", 80);
        assert!(!s.bracketed);
    }

    #[test]
    fn alt_screen_and_bracketed_paste_tracking() {
        let mut s = Scan::default();
        s.feed(b"\x1b[?1049h", 80);
        assert!(s.alt && !s.bracketed);
        s.feed(b"\x1b[?2004h", 80);
        assert!(s.alt && s.bracketed);
        s.feed(b"\x1b[?1049l\x1b[?2004l", 80);
        assert!(!s.alt && !s.bracketed);
        // old forms, combined parameters, other modes ignored
        for (on, off) in [("1047", "1047"), ("47", "47")] {
            s.feed(format!("\x1b[?{on}h").as_bytes(), 80);
            assert!(s.alt, "{on}");
            s.feed(format!("\x1b[?{off}l").as_bytes(), 80);
            assert!(!s.alt, "{off}");
        }
        s.feed(b"\x1b[?1h\x1b[4h", 80);
        assert!(!s.alt && !s.bracketed && s.mouse == 0 && !s.cursor_hidden);
        s.feed(b"\x1b[?25l\x1b[?1000;1006h", 80);
        assert!(s.cursor_hidden && s.mouse == 0b01001);
        s.feed(b"\x1b[?25h\x1b[?1000l", 80);
        assert!(!s.cursor_hidden && s.mouse == 0b01000);
        s.feed(b"\x1b[?1006l", 80);
        assert_eq!(s.mouse, 0);
        s.feed(b"\x1b[?1049;2004h", 80);
        assert!(s.alt && s.bracketed);
        // split across reads
        s.feed(b"\x1b[?10", 80);
        s.feed(b"49l", 80);
        assert!(!s.alt);
        // a new command starts on the main screen again
        s.feed(b"\x1b[?1049h\x1b]133;D\x07", 80);
        assert!(!s.alt);
    }

    #[test]
    fn wraps() {
        let mut s = Scan::default();
        s.feed(&[b'a'; 25], 10);
        assert_eq!(s.col, 5);
        s.feed(&[b'a'; 5], 10);
        assert_eq!(s.column(10), 0);
    }
}
