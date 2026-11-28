//! The ask key: finding it in the keyboard stream, and the line-prefix toggle.

use crate::config::Setting;
use crate::llm::render::unsafe_cmd;

/// The key that asks the model: Ctrl plus a letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AskKey {
    letter: u8,
}

impl AskKey {
    pub const DEFAULT: AskKey = AskKey { letter: b'q' };

    /// `ctrl-<letter>` (case-insensitive, `ctrl+x` too) or `off`.
    pub fn parse(s: &str) -> Result<Option<AskKey>, String> {
        let t = s.trim().to_ascii_lowercase();
        if t == "off" {
            return Ok(None);
        }
        let l = t
            .strip_prefix("ctrl-")
            .or_else(|| t.strip_prefix("ctrl+"))
            .filter(|l| l.len() == 1)
            .map(|l| l.as_bytes()[0])
            .filter(u8::is_ascii_lowercase)
            .ok_or_else(|| format!("keys.ask: '{s}' is not ctrl-<letter> or off"))?;
        // Tab, Enter, Backspace, Ctrl-C and Ctrl-J cannot be given away
        if matches!(l, b'c' | b'h' | b'i' | b'j' | b'm') {
            return Err(format!(
                "keys.ask: ctrl-{} is taken by the terminal",
                l as char
            ));
        }
        Ok(Some(AskKey { letter: l }))
    }

    /// The configured key; a bad value is logged and the default is used.
    pub fn from_config(s: &Setting<String>) -> Option<AskKey> {
        match s {
            Setting::Unset => Some(AskKey::DEFAULT),
            Setting::Off => None,
            Setting::Value(v) => AskKey::parse(v).unwrap_or_else(|e| {
                tracing::warn!("{e}");
                Some(AskKey::DEFAULT)
            }),
        }
    }

    /// The editor key bound to the same press.
    pub fn editor_key(self) -> aish_editor::Key {
        aish_editor::Key::Ctrl(self.letter)
    }

    /// The control byte (0x11 for ctrl-q).
    pub fn byte(self) -> u8 {
        self.letter - b'a' + 1
    }

    /// The first press in `buf`: the raw byte or the kitty/CSI-u form
    /// (`ESC [ 113 ; 5 u`). Returns its byte range. Stateless: sequences
    /// split across reads are the [`Detector`]'s job.
    pub fn find(self, buf: &[u8]) -> Option<(usize, usize)> {
        let b = self.byte();
        buf.iter().enumerate().find_map(|(i, &c)| {
            if c == b {
                Some((i, i + 1))
            } else if c == 0x1b {
                match self.csi_u(&buf[i..]) {
                    M::Full(n) => Some((i, i + n)),
                    _ => None,
                }
            } else {
                None
            }
        })
    }

    /// `ESC [ code ; mods[:event] u` with the Ctrl bit set and no modifier
    /// but Caps/Num Lock (`5`, `69`, `133`...).
    fn csi_u(self, d: &[u8]) -> M {
        let code = self.letter.to_string();
        if d.len() < 2 {
            return M::Partial;
        }
        if d[1] != b'[' {
            return M::No;
        }
        let mut i = 2;
        while i < d.len() {
            match d[i] {
                b'0'..=b'9' | b';' | b':' => i += 1,
                b'u' => {
                    let body = std::str::from_utf8(&d[2..i]).unwrap_or("");
                    let mut f = body.split(';');
                    let c = f.next().unwrap_or("").split(':').next();
                    let mut m = f.next().unwrap_or("").split(':');
                    let mods = m.next().and_then(|x| x.parse::<u32>().ok());
                    let ev = m.next().map_or(Some(1), |x| x.parse::<u32>().ok());
                    let ok = c == Some(code.as_str())
                        && mods.is_some_and(|x| {
                            let b = x.saturating_sub(1);
                            x >= 1 && b & 4 != 0 && b & !(4 | 64 | 128) == 0
                        })
                        && matches!(ev, Some(1 | 2));
                    return if ok { M::Full(i + 1) } else { M::No };
                }
                _ => return M::No,
            }
        }
        // out of bytes: could still become ours
        let body = &d[2..];
        let first = body.split(|&c| c == b';').next().unwrap_or(&[]);
        let started = body.contains(&b';');
        let code_ok = if started {
            first.split(|&c| c == b':').next() == Some(code.as_bytes())
        } else {
            code.as_bytes().starts_with(first)
        };
        if code_ok && d.len() < 24 {
            M::Partial
        } else {
            M::No
        }
    }
}

enum M {
    No,
    Partial,
    Full(usize),
}

/// A press of the ask key found in the keyboard stream.
#[derive(Debug, PartialEq, Eq)]
pub struct AskReq {
    /// Bytes typed before the key: they belong to the program.
    pub before: Vec<u8>,
    /// Bytes typed after it: the first input of the overlay.
    pub after: Vec<u8>,
    /// The key as the terminal sent it (what "a literal Ctrl-Q" is).
    pub key: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Scan {
    /// Nothing found, nothing held: forward the bytes untouched.
    Clear,
    /// Forward these bytes instead (some bytes may be held back).
    Forward(Vec<u8>),
    Hit(AskReq),
}

/// Finds the ask key in bytes bound for the program. The common path is one
/// pass over the chunk without allocation. Only the start of a CSI-u
/// sequence at the very end of a read (three bytes or more, so a lone Escape
/// is never delayed) is held back until the next read or the Esc timeout.
#[derive(Default)]
pub struct Detector {
    held: Vec<u8>,
    /// Inside a bracketed paste (`ESC[200~` .. `ESC[201~`): pasted text never
    /// triggers the key.
    paste: bool,
}

const PASTE_ON: &[u8] = b"\x1b[200~";
const PASTE_OFF: &[u8] = b"\x1b[201~";

impl Detector {
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Forget the state (the prompt took over the keyboard).
    pub fn reset(&mut self) -> Vec<u8> {
        self.paste = false;
        std::mem::take(&mut self.held)
    }

    /// Bytes that were waiting for the rest of a sequence (Esc timeout).
    pub fn take_held(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }

    pub fn scan(&mut self, key: AskKey, buf: &[u8]) -> Scan {
        let b = key.byte();
        if self.held.is_empty() && !buf.iter().any(|&c| c == 0x1b || (c == b && !self.paste)) {
            return Scan::Clear;
        }
        let had_held = !self.held.is_empty();
        let joined;
        let data: &[u8] = if had_held {
            joined = [&self.held[..], buf].concat();
            &joined
        } else {
            buf
        };
        let mut hold_at = None;
        let mut skip = 0;
        for (i, &c) in data.iter().enumerate() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let n = if c == b && !self.paste {
                1
            } else if c == 0x1b {
                let rest = &data[i..];
                if rest.starts_with(PASTE_ON) || rest.starts_with(PASTE_OFF) {
                    self.paste = rest.starts_with(PASTE_ON);
                    skip = PASTE_ON.len() - 1;
                    continue;
                }
                let marker_start = rest.len() < PASTE_ON.len()
                    && (PASTE_ON.starts_with(rest) || PASTE_OFF.starts_with(rest));
                if marker_start && rest.len() >= 3 {
                    hold_at = Some(i);
                    break;
                }
                if self.paste {
                    continue;
                }
                match key.csi_u(rest) {
                    M::Full(n) => n,
                    M::Partial if data.len() - i >= 3 => {
                        hold_at = Some(i);
                        break;
                    }
                    _ => continue,
                }
            } else {
                continue;
            };
            self.held.clear();
            return Scan::Hit(AskReq {
                before: data[..i].to_vec(),
                after: data[i + n..].to_vec(),
                key: data[i..i + n].to_vec(),
            });
        }
        match hold_at {
            Some(h) => {
                let fwd = data[..h].to_vec();
                self.held = data[h..].to_vec();
                Scan::Forward(fwd)
            }
            None if had_held => {
                let all = data.to_vec();
                self.held.clear();
                Scan::Forward(all)
            }
            None => Scan::Clear,
        }
    }
}

/// What to type into the program for a command from the model, or `None`
/// when it must only be shown: a multi-line command needs bracketed paste
/// (otherwise its newlines would act as Enter).
pub fn insert_bytes(cmd: &str, bracketed: bool) -> Option<Vec<u8>> {
    let c = cmd.trim_end();
    if c.is_empty() || unsafe_cmd(c) {
        return None;
    }
    if !c.contains('\n') {
        return Some(c.as_bytes().to_vec());
    }
    bracketed.then(|| {
        let mut v = b"\x1b[200~".to_vec();
        v.extend_from_slice(c.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    })
}

/// Toggle the `? ` marker (`p` is the configured prefix) at the start of the
/// line (the editor action). Returns the new text and cursor; the cursor
/// stays on the same character.
pub fn toggle_prefix(text: &str, cursor: usize, p: char) -> (String, usize) {
    let marker = format!("{p} ");
    if let Some(rest) = text
        .strip_prefix(marker.as_str())
        .or_else(|| text.strip_prefix(p).filter(|r| r.is_empty()))
    {
        let cut = text.len() - rest.len();
        (rest.to_owned(), cursor.saturating_sub(cut))
    } else {
        (format!("{marker}{text}"), cursor + marker.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: AskKey = AskKey { letter: b'q' };

    fn hit(s: Scan) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        match s {
            Scan::Hit(r) => (r.before, r.after, r.key),
            other => panic!("expected a hit, got {other:?}"),
        }
    }

    #[test]
    fn parse_key() {
        assert_eq!(AskKey::parse("ctrl-q"), Ok(Some(Q)));
        assert_eq!(AskKey::parse(" Ctrl+G "), Ok(Some(AskKey { letter: b'g' })));
        assert_eq!(AskKey::parse("off"), Ok(None));
        for bad in [
            "q", "ctrl-", "ctrl-qq", "alt-q", "ctrl-1", "ctrl-c", "ctrl-m", "ctrl-i",
        ] {
            assert!(AskKey::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Q.byte(), 0x11);
        assert_eq!(AskKey::from_config(&Setting::Unset), Some(Q));
        assert_eq!(AskKey::from_config(&Setting::Off), None);
        assert_eq!(AskKey::from_config(&Setting::Value("nope".into())), Some(Q));
    }

    #[test]
    fn raw_byte_and_csi_u() {
        let mut d = Detector::default();
        assert_eq!(d.scan(Q, b"hello"), Scan::Clear);
        assert_eq!(d.scan(Q, b"\x1b[A\x1b[1;5C"), Scan::Clear); // other keys
        assert_eq!(
            hit(d.scan(Q, b"ab\x11cd")),
            (b"ab".to_vec(), b"cd".to_vec(), vec![0x11])
        );
        assert_eq!(
            hit(d.scan(Q, b"\x1b[113;5u")),
            (vec![], vec![], b"\x1b[113;5u".to_vec())
        );
        assert_eq!(
            hit(d.scan(Q, b"x\x1b[113;5:1uy")),
            (b"x".to_vec(), b"y".to_vec(), b"\x1b[113;5:1u".to_vec())
        );
        // other letters / modifiers are not ours
        assert_eq!(d.scan(Q, b"\x1b[119;5u\x1b[113;3u\x1b[113u"), Scan::Clear);
        assert_eq!(AskKey::find(Q, b"\x1b[113;5u"), Some((0, 8)));
    }

    #[test]
    fn csi_u_split_across_reads() {
        let mut d = Detector::default();
        // everything but a lone Escape / `ESC [` is held back, then completed
        assert_eq!(d.scan(Q, b"ab\x1b[11"), Scan::Forward(b"ab".to_vec()));
        assert!(!d.is_empty());
        assert_eq!(
            hit(d.scan(Q, b"3;5uzz")),
            (vec![], b"zz".to_vec(), b"\x1b[113;5u".to_vec())
        );
        assert!(d.is_empty());
        // three pieces
        assert_eq!(d.scan(Q, b"\x1b[113"), Scan::Forward(vec![]));
        assert_eq!(d.scan(Q, b";5"), Scan::Forward(vec![]));
        assert_eq!(hit(d.scan(Q, b"u")).2, b"\x1b[113;5u");
        // the held start turns out to be something else: it is released
        assert_eq!(d.scan(Q, b"\x1b[11"), Scan::Forward(vec![]));
        assert_eq!(d.scan(Q, b"~x"), Scan::Forward(b"\x1b[11~x".to_vec()));
        assert!(d.is_empty());
        // Esc timeout releases what is held
        assert_eq!(d.scan(Q, b"\x1b[113;"), Scan::Forward(vec![]));
        assert_eq!(d.take_held(), b"\x1b[113;");
        assert!(d.is_empty());
        // a lone Escape (or `ESC [`) is never held: no added latency for vim
        assert_eq!(d.scan(Q, b"\x1b"), Scan::Clear);
        assert_eq!(d.scan(Q, b"\x1b["), Scan::Clear);
        assert!(d.is_empty());
    }

    #[test]
    fn csi_u_modifiers_and_two_digit_codes() {
        let a = AskKey { letter: b'a' };
        assert_eq!(a.find(b"\x1b[97;5u"), Some((0, 7))); // no zero padding
        assert_eq!(a.find(b"\x1b[097;5u"), None);
        // Ctrl with Caps/Num Lock, and the event-type form
        for m in ["5", "69", "133", "197", "5:1", "5:2"] {
            let seq = format!("\x1b[113;{m}u");
            assert_eq!(Q.find(seq.as_bytes()), Some((0, seq.len())), "{m}");
        }
        // Ctrl-Alt/Shift/Super, no Ctrl, other keys: not ours
        for m in ["1", "3", "6", "7", "13", "5:3"] {
            assert_eq!(Q.find(format!("\x1b[113;{m}u").as_bytes()), None, "{m}");
        }
        let mut d = Detector::default();
        assert_eq!(d.scan(a, b"\x1b[9"), Scan::Forward(vec![]));
        assert_eq!(hit(d.scan(a, b"7;69u")).2, b"\x1b[97;69u");
    }

    #[test]
    fn pasted_key_does_not_open_the_overlay() {
        let mut d = Detector::default();
        assert_eq!(d.scan(Q, b"\x1b[200~ab\x11cd\x1b[113;5u"), Scan::Clear);
        assert_eq!(d.scan(Q, b"more\x11"), Scan::Clear); // still inside
        let (before, after, _) = hit(d.scan(Q, b"\x1b[201~x\x11y"));
        assert_eq!((before, after), (b"\x1b[201~x".to_vec(), b"y".to_vec()));
        // the marker split across reads
        assert_eq!(d.scan(Q, b"\x1b[20"), Scan::Forward(vec![]));
        assert_eq!(
            d.scan(Q, b"0~\x11"),
            Scan::Forward(b"\x1b[200~\x11".to_vec())
        );
        assert_eq!(d.reset(), Vec::<u8>::new());
    }

    #[test]
    fn insert_decision() {
        let ins = |c: &str, bp| insert_bytes(c, bp);
        // single line: typed as is, with and without bracketed paste, no newline
        assert_eq!(ins("ls -l\n", false), Some(b"ls -l".to_vec()));
        assert_eq!(ins("ls -l", true), Some(b"ls -l".to_vec()));
        // multi-line: only as a bracketed paste
        assert_eq!(ins("a \\\n  b", false), None);
        assert_eq!(
            ins("a \\\n  b", true),
            Some(b"\x1b[200~a \\\n  b\x1b[201~".to_vec())
        );
        // never Enter, never escapes
        assert_eq!(ins("", true), None);
        assert_eq!(ins("echo \x1b[31m", true), None);
        assert_eq!(ins("echo a\rb", true), None);
        for c in ["ls", "a\nb"] {
            let v = ins(c, true).unwrap();
            assert!(!v.ends_with(b"\n") && !v.ends_with(b"\r"));
        }
    }

    #[test]
    fn prefix_toggle() {
        assert_eq!(toggle_prefix("ls", 2, '?'), ("? ls".into(), 4));
        assert_eq!(toggle_prefix("ls", 0, '?'), ("? ls".into(), 2));
        assert_eq!(toggle_prefix("", 0, '?'), ("? ".into(), 2));
        assert_eq!(toggle_prefix("? ls", 4, '?'), ("ls".into(), 2));
        assert_eq!(toggle_prefix("? ls", 1, '?'), ("ls".into(), 0));
        assert_eq!(toggle_prefix("? ", 2, '?'), ("".into(), 0));
        assert_eq!(toggle_prefix("?", 1, '?'), ("".into(), 0));
        assert_eq!(toggle_prefix("ls", 2, ','), (", ls".into(), 4));
        assert_eq!(toggle_prefix(", ls", 4, ','), ("ls".into(), 2));
        assert_eq!(toggle_prefix("? ls", 4, ','), (", ? ls".into(), 6));
        assert_eq!(toggle_prefix("?x", 2, '?'), ("? ?x".into(), 4)); // a glob stays a command
    }
}
