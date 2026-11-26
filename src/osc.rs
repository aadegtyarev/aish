//! Private OSC sequences emitted by the shell integration, and the filter that
//! keeps them away from the user's terminal.

/// Everything llmsh-private starts with this prefix.
pub const PRIVATE_PREFIX: &[u8] = b"\x1b]1337;llmsh;";

#[derive(Clone, Copy)]
enum State {
    Normal,
    /// Matched this many bytes of the prefix.
    Matching(usize),
    /// Inside a private OSC; bool = previous byte was ESC.
    Inside(bool),
}

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
                            State::Inside(false)
                        } else {
                            State::Matching(n + 1)
                        };
                    } else {
                        // Not ours: release held bytes, re-examine `b`.
                        out.extend_from_slice(&PRIVATE_PREFIX[..n]);
                        self.state = State::Normal;
                    }
                }
                State::Inside(esc) => {
                    let b = input[i];
                    i += 1;
                    self.state = if b == 0x07 || (esc && b == b'\\') {
                        State::Normal
                    } else {
                        State::Inside(b == 0x1b)
                    };
                }
            }
        }
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
        let s = b"a\x1b]133;A\x07b\x1b]1337;llmsh;t=ls\x07c\x1b[0md\x1b]1337;llmsh;b=eA==\x1b\\e";
        assert_eq!(run(&[s]), b"a\x1b]133;A\x07bc\x1b[0mde");
    }

    #[test]
    fn split_at_every_byte() {
        let s = b"x\x1b]1337;llmsh;t=ls\x07y\x1b]7;file:///\x07z\x1b[1m";
        let chunks: Vec<&[u8]> = s.chunks(1).collect();
        assert_eq!(run(&chunks), b"xy\x1b]7;file:///\x07z\x1b[1m");
    }

    #[test]
    fn near_miss_prefix_released() {
        assert_eq!(
            run(&[b"\x1b]1337;llm", b"x\x1b\x1b[0m"]),
            b"\x1b]1337;llmx\x1b\x1b[0m"
        );
    }
}
