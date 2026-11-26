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
    osc: [u8; 8],
    osc_n: usize,
    utf: [u8; 4],
    utf_n: usize,
    utf_need: usize,
    /// Prompt printed (133;B) and no command started since.
    pub b_seen: bool,
    /// 133;A seen since last cleared (a prompt is being (re)drawn).
    pub saw_a: bool,
    /// 133;C or D seen since last cleared.
    pub saw_cd: bool,
}

impl Default for Scan {
    fn default() -> Self {
        Scan {
            col: 0,
            st: St::Ground,
            osc: [0; 8],
            osc_n: 0,
            utf: [0; 4],
            utf_n: 0,
            utf_need: 0,
            b_seen: false,
            saw_a: false,
            saw_cd: false,
        }
    }
}

impl Scan {
    pub fn feed(&mut self, data: &[u8], width: u16) {
        let width = width.max(1);
        for &b in data {
            match self.st {
                St::Ground => self.ground(b, width),
                St::Esc => {
                    self.st = match b {
                        b'[' => St::Csi,
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
                        self.st = St::Ground;
                    } else if b == 0x1b {
                        self.st = St::Esc;
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
        if o.len() >= 5 && &o[..4] == b"133;" && (o.len() == 5 || o[5] == b';') {
            match o[4] {
                b'A' => {
                    self.b_seen = false;
                    self.saw_a = true;
                }
                b'B' => self.b_seen = true,
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
    fn wraps() {
        let mut s = Scan::default();
        s.feed(&[b'a'; 25], 10);
        assert_eq!(s.col, 5);
        s.feed(&[b'a'; 5], 10);
        assert_eq!(s.column(10), 0);
    }
}
