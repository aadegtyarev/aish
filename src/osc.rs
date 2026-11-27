//! Private OSC sequences emitted by the shell integration, and the filter that
//! keeps them away from the user's terminal.

/// Everything aish-private starts with this prefix.
pub const PRIVATE_PREFIX: &[u8] = b"\x1b]1337;aish;";

#[derive(Clone, Copy)]
enum State {
    Normal,
    /// Matched this many bytes of the prefix.
    Matching(usize),
    /// Inside a private OSC (bytes so far), previous byte was ESC.
    Inside(usize, bool),
}

/// Longest private OSC we will swallow; beyond this it is not ours.
const MAX_OSC: usize = 64 * 1024;

/// Streaming filter removing private OSCs from forwarded output. Only holds
/// bytes back while they might still turn out to be the private prefix.
pub struct Filter {
    state: State,
}

impl Default for Filter {
    fn default() -> Self {
        Filter {
            state: State::Normal,
        }
    }
}

impl Filter {
    pub fn filter(&mut self, input: &[u8], out: &mut Vec<u8>) {
        let mut i = 0;
        while i < input.len() {
            match self.state {
                State::Normal => {
                    // Bulk-copy up to the next ESC.
                    let n = input[i..]
                        .iter()
                        .position(|&b| b == 0x1b)
                        .unwrap_or(input.len() - i);
                    out.extend_from_slice(&input[i..i + n]);
                    i += n;
                    if i < input.len() {
                        self.state = State::Matching(0);
                    }
                }
                State::Matching(n) => {
                    let b = input[i];
                    if b == PRIVATE_PREFIX[n] {
                        i += 1;
                        self.state = if n + 1 == PRIVATE_PREFIX.len() {
                            State::Inside(0, false)
                        } else {
                            State::Matching(n + 1)
                        };
                    } else {
                        // Not ours: release held bytes, re-examine `b`.
                        out.extend_from_slice(&PRIVATE_PREFIX[..n]);
                        self.state = State::Normal;
                    }
                }
                State::Inside(len, esc) => {
                    let b = input[i];
                    if esc && b != b'\\' {
                        // ESC began something new (e.g. the next OSC): re-examine.
                        self.state = State::Matching(1);
                        continue;
                    }
                    i += 1;
                    self.state = match b {
                        0x07 | 0x18 | 0x1a => State::Normal,
                        b'\\' if esc => State::Normal,
                        _ if len >= MAX_OSC => State::Normal,
                        _ => State::Inside(len + 1, b == 0x1b),
                    };
                }
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
enum G {
    #[default]
    Normal,
    Esc,
    Osc(usize),
    OscEsc,
}

/// Tracks whether a stream is inside an OSC and for how long, so the vte
/// parser (which buffers OSCs without bound) can be told to give up.
#[derive(Default)]
pub struct OscGuard {
    state: G,
}

impl OscGuard {
    /// Scan `bytes`; returns the index of the byte at which an OSC exceeded
    /// the cap (state is reset, call again with the rest).
    pub fn advance(&mut self, bytes: &[u8]) -> Option<usize> {
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            self.state = match self.state {
                G::Normal => {
                    match bytes[i..].iter().position(|&x| x == 0x1b) {
                        Some(n) => i += n,
                        None => return None,
                    }
                    G::Esc
                }
                G::Esc if b == b']' => G::Osc(0),
                G::Esc if b == 0x1b => G::Esc,
                G::Esc => G::Normal,
                G::Osc(n) => match b {
                    0x07 | 0x18 | 0x1a => G::Normal,
                    0x1b => G::OscEsc,
                    _ if n >= MAX_OSC => {
                        self.state = G::Normal;
                        return Some(i);
                    }
                    _ => G::Osc(n + 1),
                },
                G::OscEsc if b == b']' => G::Osc(0),
                G::OscEsc => G::Normal,
            };
            i += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut f = Filter::default();
        let mut out = Vec::new();
        for c in chunks {
            f.filter(c, &mut out);
        }
        out
    }

    #[test]
    fn strips_private_keeps_others() {
        let s = b"a\x1b]133;A\x07b\x1b]1337;aish;t=ls\x07c\x1b[0md\x1b]1337;aish;b=eA==\x1b\\e";
        assert_eq!(run(&[s]), b"a\x1b]133;A\x07bc\x1b[0mde");
    }

    #[test]
    fn split_at_every_byte() {
        let s = b"x\x1b]1337;aish;t=ls\x07y\x1b]7;file:///\x07z\x1b[1m";
        let chunks: Vec<&[u8]> = s.chunks(1).collect();
        assert_eq!(run(&chunks), b"xy\x1b]7;file:///\x07z\x1b[1m");
    }

    #[test]
    fn unterminated_private_osc_is_released() {
        // CAN, ESC-reexamine and the length cap all end the swallowing.
        assert_eq!(run(&[b"a\x1b]1337;aish;xx\x18b"]), b"ab");
        assert_eq!(run(&[b"a\x1b]1337;aish;xx\x1b[0mb"]), b"a\x1b[0mb");
        let mut big = b"\x1b]1337;aish;".to_vec();
        big.extend(vec![b'z'; 70 * 1024]);
        big.extend(b"tail");
        let out = run(&[&big]);
        assert!(out.ends_with(b"tail") && out.len() < 70 * 1024);
    }

    #[test]
    fn guard_caps_osc() {
        let mut g = OscGuard::default();
        assert_eq!(g.advance(b"\x1b]0;title\x07plain"), None);
        let mut v = b"\x1b]x".to_vec();
        v.extend(vec![b'a'; MAX_OSC + 10]);
        assert!(g.advance(&v).is_some());
    }

    #[test]
    fn near_miss_prefix_released() {
        assert_eq!(
            run(&[b"\x1b]1337;llm", b"x\x1b\x1b[0m"]),
            b"\x1b]1337;llmx\x1b\x1b[0m"
        );
    }
}
