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

pub struct Scan {
    pub col: u16,
    st: St,
    osc: [u8; 64],
    osc_n: usize,
    csi: [u8; 8],
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
    /// bash has bracketed paste on (it sent `CSI ? 2004 h`).
    pub bracketed: bool,
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
            st: St::Ground,
            osc: [0; 64],
            osc_n: 0,
            csi: [0; 8],
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
                        if &self.csi[..self.csi_n] == b"?2004" {
                            match b {
                                b'h' => self.bracketed = true,
                                b'l' => self.bracketed = false,
                                _ => {}
                            }
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

    fn end_osc(&mut self) {
        self.st = St::Ground;
        let o = &self.osc[..self.osc_n];
        let tag = format!(";aish={}", self.nonce);
        let ours = self.nonce.is_empty() || o.ends_with(tag.as_bytes());
        if ours && o.len() >= 5 && &o[..4] == b"133;" && (o.len() == 5 || o[5] == b';') {
            match o[4] {
                b'A' => {
                    self.b_seen = false;
                    self.saw_a = true;
                }
                b'B' => {
                    self.b_seen = true;
                    self.saw_b = true;
                    self.printed_after_b = false;
                }
                b'C' | b'D' => {
                    self.b_seen = false;
                    self.saw_cd = true;
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
            self.col = w;
        } else {
            self.col += w;
        }
    }

    fn ground(&mut self, b: u8, width: u16) {
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
            b'\n' | 0x0b | 0x0c => {}
            0x08 => self.col = self.col.saturating_sub(1),
            b'\t' => self.col = ((self.col / 8 + 1) * 8).min(width - 1),
            0x00..=0x1f | 0x7f => {}
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
    fn wraps() {
        let mut s = Scan::default();
        s.feed(&[b'a'; 25], 10);
        assert_eq!(s.col, 5);
        s.feed(&[b'a'; 5], 10);
        assert_eq!(s.column(10), 0);
    }
}
