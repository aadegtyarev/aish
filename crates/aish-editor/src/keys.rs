//! Byte stream -> keys. Handles sequences split across reads, UTF-8, SS3/CSI,
//! modifiers, bracketed paste and cursor-position reports.

/// A decoded key press.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Key {
    /// A printable character.
    Char(char),
    /// Control letter (`b'a'..=b'z'`), `b'_'` for Ctrl-_ and `b' '` for Ctrl-Space.
    Ctrl(u8),
    /// Alt/Meta plus a character.
    Alt(char),
    /// Enter / Return.
    Enter,
    /// Tab.
    Tab,
    /// Shift-Tab.
    BackTab,
    /// Backspace (also Ctrl-H).
    Backspace,
    /// Alt-Backspace.
    AltBackspace,
    /// Delete.
    Delete,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Ctrl-Left.
    CtrlLeft,
    /// Ctrl-Right.
    CtrlRight,
    /// Alt-Left.
    AltLeft,
    /// Alt-Right.
    AltRight,
    /// Escape.
    Esc,
    /// Bracketed paste contents.
    Paste(String),
    /// Reply to a cursor-position query: (row, col), 1-based.
    CursorPos(u16, u16),
}

const PASTE_END: &[u8] = b"\x1b[201~";
const MAX_PENDING: usize = 4096;

#[derive(Default)]
pub(crate) struct Decoder {
    pending: Vec<u8>,
    in_paste: bool,
    paste: Vec<u8>,
}

enum Esc {
    Incomplete,
    /// Consumed bytes and the resulting key, if any.
    Done(usize, Option<Key>),
    PasteStart(usize),
}

/// Result of decoding at the start of a byte slice.
pub(crate) enum One {
    /// A key and the number of bytes it used.
    Key(Key, usize),
    /// Bytes consumed without producing a key (ignored sequence, paste data).
    Skip(usize),
    /// The slice is a partial sequence; wait for more bytes.
    Incomplete,
}

impl Decoder {
    pub(crate) fn new() -> Self {
        Decoder {
            pending: Vec::with_capacity(64),
            in_paste: false,
            paste: Vec::new(),
        }
    }

    /// A partial sequence (typically a lone ESC) is waiting for more bytes.
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    pub(crate) fn set_pending(&mut self, tail: &[u8]) {
        self.pending.clear();
        if tail.len() <= MAX_PENDING {
            self.pending.extend_from_slice(tail);
        }
    }

    /// Input went quiet: a held back ESC was a real Escape; other partial
    /// sequences are dropped.
    pub(crate) fn flush(&mut self) -> Option<Key> {
        let esc = self.pending == [0x1b];
        self.pending.clear();
        esc.then_some(Key::Esc)
    }

    /// Decode at the start of `data` (non-empty).
    pub(crate) fn parse_one(&mut self, data: &[u8]) -> One {
        if self.in_paste {
            return match find(data, PASTE_END) {
                Some(p) => {
                    self.paste.extend_from_slice(&data[..p]);
                    self.in_paste = false;
                    let s = String::from_utf8_lossy(&self.paste).into_owned();
                    self.paste.clear();
                    One::Key(Key::Paste(s), p + PASTE_END.len())
                }
                None => {
                    // Keep a possible partial terminator for the next read.
                    let keep = (1..PASTE_END.len().min(data.len() + 1))
                        .rev()
                        .find(|&k| data.ends_with(&PASTE_END[..k]))
                        .unwrap_or(0);
                    self.paste.extend_from_slice(&data[..data.len() - keep]);
                    if keep == data.len() {
                        One::Incomplete
                    } else {
                        One::Skip(data.len() - keep)
                    }
                }
            };
        }
        let b = data[0];
        let key = |k: Key| One::Key(k, 1);
        match b {
            0x1b => match escape(data) {
                Esc::Incomplete => One::Incomplete,
                Esc::Done(n, Some(k)) => One::Key(k, n),
                Esc::Done(n, None) => One::Skip(n),
                Esc::PasteStart(n) => {
                    self.in_paste = true;
                    One::Skip(n)
                }
            },
            b'\r' | b'\n' => key(Key::Enter),
            b'\t' => key(Key::Tab),
            0x08 | 0x7f => key(Key::Backspace),
            0 => key(Key::Ctrl(b' ')),
            0x1f => key(Key::Ctrl(b'_')),
            1..=26 => key(Key::Ctrl(b + b'a' - 1)),
            0x1c..=0x1e => One::Skip(1),
            0x20..=0x7e => key(Key::Char(b as char)),
            _ => match utf8(data) {
                Utf8::Incomplete => One::Incomplete,
                Utf8::Bad => One::Skip(1),
                Utf8::Ok(c, n) => One::Key(Key::Char(c), n),
            },
        }
    }

    /// Test helper: decode a whole chunk, keeping partial tails.
    #[cfg(test)]
    pub(crate) fn feed(&mut self, bytes: &[u8], f: &mut impl FnMut(Key)) {
        let mut data = self.take_pending();
        data.extend_from_slice(bytes);
        let mut i = 0;
        while i < data.len() {
            match self.parse_one(&data[i..]) {
                One::Key(k, n) => {
                    i += n;
                    f(k);
                }
                One::Skip(n) => i += n,
                One::Incomplete => break,
            }
        }
        self.set_pending(&data[i..]);
    }
}

enum Utf8 {
    Incomplete,
    Bad,
    Ok(char, usize),
}

fn utf8(d: &[u8]) -> Utf8 {
    let n = match d[0] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Utf8::Bad,
    };
    if d.len() < n {
        // Valid so far?
        return if d[1..].iter().all(|&b| b & 0xc0 == 0x80) {
            Utf8::Incomplete
        } else {
            Utf8::Bad
        };
    }
    match std::str::from_utf8(&d[..n]) {
        Ok(s) => Utf8::Ok(s.chars().next().unwrap_or('\u{fffd}'), n),
        Err(_) => Utf8::Bad,
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// `d[0]` is ESC.
fn escape(d: &[u8]) -> Esc {
    if d.len() == 1 {
        return Esc::Incomplete;
    }
    match d[1] {
        b'[' => csi(d),
        b'O' => {
            if d.len() < 3 {
                return Esc::Incomplete;
            }
            let k = match d[2] {
                b'A' => Some(Key::Up),
                b'B' => Some(Key::Down),
                b'C' => Some(Key::Right),
                b'D' => Some(Key::Left),
                b'H' => Some(Key::Home),
                b'F' => Some(Key::End),
                _ => None,
            };
            Esc::Done(3, k)
        }
        0x1b => Esc::Done(1, Some(Key::Esc)),
        0x7f | 0x08 => Esc::Done(2, Some(Key::AltBackspace)),
        0x20..=0x7e => Esc::Done(2, Some(Key::Alt(d[1] as char))),
        0x80..=0xff => match utf8(&d[1..]) {
            Utf8::Incomplete => Esc::Incomplete,
            Utf8::Bad => Esc::Done(2, None),
            Utf8::Ok(c, n) => Esc::Done(1 + n, Some(Key::Alt(c))),
        },
        _ => Esc::Done(2, None),
    }
}

fn csi(d: &[u8]) -> Esc {
    let mut i = 2;
    while i < d.len() && (0x30..=0x3f).contains(&d[i]) {
        i += 1;
    }
    while i < d.len() && (0x20..=0x2f).contains(&d[i]) {
        i += 1;
    }
    if i >= d.len() {
        return Esc::Incomplete;
    }
    let fin = d[i];
    if !(0x40..=0x7e).contains(&fin) {
        return Esc::Done(i, None); // malformed: drop what we saw
    }
    let params = &d[2..i];
    let n = i + 1;
    let mut nums = [0u16; 3];
    for (slot, part) in nums.iter_mut().zip(params.split(|&b| b == b';')) {
        *slot = std::str::from_utf8(part)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
    }
    let modifier = nums[1];
    let key = match fin {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'C' => Some(match modifier {
            5 => Key::CtrlRight,
            3 | 4 | 7 => Key::AltRight,
            _ => Key::Right,
        }),
        b'D' => Some(match modifier {
            5 => Key::CtrlLeft,
            3 | 4 | 7 => Key::AltLeft,
            _ => Key::Left,
        }),
        b'H' => Some(Key::Home),
        b'F' => Some(Key::End),
        b'Z' => Some(Key::BackTab),
        b'R' if params.contains(&b';') && !params.starts_with(b"?") => {
            Some(Key::CursorPos(nums[0], nums[1]))
        }
        b'~' => match nums[0] {
            1 | 7 => Some(Key::Home),
            4 | 8 => Some(Key::End),
            3 => Some(Key::Delete),
            200 => return Esc::PasteStart(n),
            _ => None,
        },
        _ => None,
    };
    Esc::Done(n, key)
}

/// Removes cursor-position replies (`ESC [ row ; col R`) from forwarded
/// input, also when a reply is split across reads.
#[derive(Default)]
pub struct CursorReportFilter {
    held: Vec<u8>,
}

impl CursorReportFilter {
    /// Filter `input` into `out` (appended); returns whether a reply was
    /// removed. A trailing partial reply is held back until the next call.
    pub fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) -> bool {
        let mut data = std::mem::take(&mut self.held);
        data.extend_from_slice(input);
        let mut found = false;
        let mut r = 0;
        while r < data.len() {
            if data[r] == 0x1b && data.get(r + 1).is_none_or(|&b| b == b'[') {
                let mut j = r + 2;
                while j < data.len() && (data[j].is_ascii_digit() || data[j] == b';') {
                    j += 1;
                }
                if j == data.len() && j > r + 1 {
                    self.held.extend_from_slice(&data[r..]); // might still become a reply
                    break;
                }
                if j > r + 2 && data.get(j) == Some(&b'R') {
                    found = true;
                    r = j + 1;
                    continue;
                }
            }
            out.push(data[r]);
            r += 1;
        }
        found
    }

    /// Drop and return any held bytes (the reply never completed).
    pub fn take_held(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(chunks: &[&[u8]]) -> Vec<Key> {
        let mut d = Decoder::new();
        let mut out = vec![];
        for c in chunks {
            d.feed(c, &mut |k| out.push(k));
        }
        out.extend(d.flush());
        out
    }

    #[test]
    fn plain_and_ctrl() {
        assert_eq!(
            all(&[b"a\x01\r\x7f\t\x1f"]),
            vec![
                Key::Char('a'),
                Key::Ctrl(b'a'),
                Key::Enter,
                Key::Backspace,
                Key::Tab,
                Key::Ctrl(b'_')
            ]
        );
    }

    #[test]
    fn arrows_and_modifiers() {
        assert_eq!(
            all(&[b"\x1b[A\x1bOB\x1b[1;5C\x1b[1;3D\x1b[3~\x1b[H\x1b[F\x1b[Z"]),
            vec![
                Key::Up,
                Key::Down,
                Key::CtrlRight,
                Key::AltLeft,
                Key::Delete,
                Key::Home,
                Key::End,
                Key::BackTab
            ]
        );
    }

    #[test]
    fn split_everywhere() {
        let s = b"\x1b[1;5Cx\xd0\xb6\x1bb\x1b[200~a\nb\x1b[201~z";
        let whole = all(&[s]);
        for i in 1..s.len() {
            assert_eq!(all(&[&s[..i], &s[i..]]), whole, "split at {i}");
        }
        assert_eq!(
            whole,
            vec![
                Key::CtrlRight,
                Key::Char('x'),
                Key::Char('ж'),
                Key::Alt('b'),
                Key::Paste("a\nb".into()),
                Key::Char('z')
            ]
        );
    }

    #[test]
    fn lone_escape_flushes() {
        let mut d = Decoder::new();
        let mut out = vec![];
        d.feed(b"\x1b", &mut |k| out.push(k));
        assert!(out.is_empty() && d.has_pending());
        out.extend(d.flush());
        assert_eq!(out, vec![Key::Esc]);
    }

    #[test]
    fn utf8_wide_and_alt() {
        assert_eq!(
            all(&[b"\xe4\xb8\xad\xf0\x9f\x98\x80\x1b\x7f"]),
            vec![Key::Char('中'), Key::Char('😀'), Key::AltBackspace]
        );
    }

    #[test]
    fn cursor_report_and_unknown() {
        assert_eq!(
            all(&[b"\x1b[12;34R\x1b[99~\x1b[?1;2c"]),
            vec![Key::CursorPos(12, 34)]
        );
    }

    #[test]
    fn paste_split_terminator() {
        let k = all(&[b"\x1b[200~ab\x1b[20", b"1~"]);
        assert_eq!(k, vec![Key::Paste("ab".into())]);
    }

    #[test]
    fn strips_cursor_reports_even_when_split() {
        let mut f = CursorReportFilter::default();
        let mut out = vec![];
        assert!(f.filter(b"a\x1b[3;7Rb", &mut out));
        assert_eq!(out, b"ab");
        for split in 1..8 {
            if split == 2 {
                continue; // lone trailing ESC is the Escape key: never held back
            }
            let s = b"x\x1b[12;34Ry";
            let mut f = CursorReportFilter::default();
            let mut out = vec![];
            let found = f.filter(&s[..split], &mut out) | f.filter(&s[split..], &mut out);
            assert!(found, "split {split}");
            assert_eq!(out, b"xy", "split {split}");
        }
        // a lone ESC (the Escape key) is not held back
        let mut f = CursorReportFilter::default();
        let mut out = vec![];
        f.filter(b"\x1b", &mut out);
        assert_eq!(out, b"\x1b");
        // other sequences pass through
        let mut out = vec![];
        f.filter(b"\x1b[A\x1b[1;5C", &mut out);
        assert_eq!(out, b"\x1b[A\x1b[1;5C");
    }
}
