//! Command blocks built from OSC 133 marks in the PTY output stream.
//!
//! Marks: A prompt start, B input start, C output start, D;<exit> finished.
//! Private `1337;llmsh;{t,b}=<cmd>` (plain / base64) arrives just before D.

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

use vte::{Params, Parser, Perform};

use crate::osc::{OscGuard, PRIVATE_PREFIX};

const HEAD_CAP: usize = 128 * 1024;
const TAIL_CAP: usize = 128 * 1024;
const MAX_BLOCKS: usize = 100;

#[derive(Debug, Clone)]
pub struct Block {
    pub command: String,
    /// Plain text (ANSI stripped), capped to ~256 KB keeping head and tail.
    pub output: String,
    pub exit_code: Option<i32>,
    pub cwd: String,
    pub started_at: SystemTime,
    pub duration: Duration,
}

/// Ring buffer of the most recent blocks.
#[derive(Debug, Default)]
pub struct Blocks {
    ring: VecDeque<Block>,
}

impl Blocks {
    fn push(&mut self, b: Block) {
        if self.ring.len() == MAX_BLOCKS {
            self.ring.pop_front();
        }
        self.ring.push_back(b);
    }

    /// The last `n` blocks, oldest first.
    pub fn last(&self, n: usize) -> impl Iterator<Item = &Block> {
        self.ring.iter().skip(self.ring.len().saturating_sub(n))
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

/// Output accumulator that keeps the first HEAD_CAP and last TAIL_CAP bytes.
#[derive(Default)]
struct Capped {
    head: String,
    tail: String,
    omitted: usize,
}

impl Capped {
    fn push(&mut self, c: char) {
        if self.head.len() < HEAD_CAP {
            self.head.push(c);
        } else {
            self.tail.push(c);
            if self.tail.len() > 2 * TAIL_CAP {
                self.trim_tail();
            }
        }
    }

    fn trim_tail(&mut self) {
        let mut cut = self.tail.len().saturating_sub(TAIL_CAP);
        while !self.tail.is_char_boundary(cut) {
            cut += 1;
        }
        self.omitted += cut;
        self.tail.drain(..cut);
    }

    fn backspace(&mut self) {
        let s = if self.tail.is_empty() {
            &mut self.head
        } else {
            &mut self.tail
        };
        if s.chars().next_back().is_some_and(|c| c != '\n') {
            s.pop();
        }
    }

    fn finish(mut self) -> String {
        self.trim_tail();
        if self.omitted == 0 {
            self.head.push_str(&self.tail);
            return self.head;
        }
        format!(
            "{}\n[... {} bytes omitted ...]\n{}",
            self.head, self.omitted, self.tail
        )
    }
}

struct Run {
    started: Instant,
    started_at: SystemTime,
    cwd: String,
    out: Capped,
}

#[derive(Default)]
struct Collector {
    cwd: String,
    pending_cmd: Option<String>,
    run: Option<Run>,
    blocks: Blocks,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Collector {
    fn finish(&mut self, exit: Option<i32>) {
        let cmd = self.pending_cmd.take();
        if let Some(run) = self.run.take() {
            let block = Block {
                command: cmd.unwrap_or_default(),
                output: run.out.finish(),
                exit_code: exit,
                cwd: run.cwd,
                started_at: run.started_at,
                duration: run.started.elapsed(),
            };
            tracing::debug!(cmd = %block.command, exit = ?block.exit_code, ms = block.duration.as_millis() as u64, bytes = block.output.len(), "block");
            self.blocks.push(block);
        }
    }
}

impl Perform for Collector {
    fn print(&mut self, c: char) {
        if let Some(r) = &mut self.run {
            r.out.push(c);
        }
    }

    fn execute(&mut self, byte: u8) {
        if let Some(r) = &mut self.run {
            match byte {
                b'\n' => r.out.push('\n'),
                b'\t' => r.out.push('\t'),
                0x08 => r.out.backspace(),
                _ => {}
            }
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell: bool) {
        match params {
            [b"133", mark, rest @ ..] => match mark.first() {
                Some(b'C') => {
                    self.run = Some(Run {
                        started: Instant::now(),
                        started_at: SystemTime::now(),
                        cwd: self.cwd.clone(),
                        out: Capped::default(),
                    });
                }
                Some(b'D') => {
                    let exit = rest
                        .first()
                        .and_then(|p| std::str::from_utf8(p).ok()?.parse().ok());
                    self.finish(exit);
                }
                _ => {}
            },
            [b"7", url, ..] => {
                // file://host/path; ';' never appears (bash encodes only spaces).
                if let Ok(u) = std::str::from_utf8(url) {
                    if let Some(rest) = u.strip_prefix("file://") {
                        if let Some(i) = rest.find('/') {
                            self.cwd = percent_decode(&rest[i..]);
                        }
                    }
                }
            }
            [b"1337", b"llmsh", payload @ ..] => {
                // vte splits on ';', and plain commands may contain it.
                debug_assert!(PRIVATE_PREFIX.ends_with(b"llmsh;"));
                let payload = payload.join(&b';');
                let cmd = match payload.split_at_checked(2) {
                    Some((b"p=", p)) => Some(percent_decode(&String::from_utf8_lossy(p))),
                    _ => None,
                };
                if cmd.is_some() {
                    self.pending_cmd = cmd;
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}
}

/// Feed raw PTY output; query finished blocks.
pub struct BlockTracker {
    parser: Parser,
    guard: OscGuard,
    state: Collector,
}

impl Default for BlockTracker {
    fn default() -> Self {
        BlockTracker {
            parser: Parser::new(),
            guard: OscGuard::default(),
            state: Collector::default(),
        }
    }
}

impl BlockTracker {
    pub fn feed(&mut self, bytes: &[u8]) {
        // vte buffers unterminated OSCs without bound; cut them off with CAN.
        let mut rest = bytes;
        while let Some(i) = self.guard.advance(rest) {
            self.parser.advance(&mut self.state, &rest[..=i]);
            self.parser.advance(&mut self.state, &[0x18]);
            rest = &rest[i + 1..];
        }
        self.parser.advance(&mut self.state, rest);
    }

    pub fn blocks(&self) -> &Blocks {
        &self.state.blocks
    }

    pub fn cwd(&self) -> &str {
        &self.state.cwd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &[u8] = b"\x1b]7;file://h/tmp/a%20b\x07\x1b]133;A\x07$ \x1b]133;B\x07\
\x1b]133;C\x07\x1b[1mhi\x1b[0m\r\nline\ttwo\r\n\
\x1b]1337;llmsh;p=echo%20hi;%20x\x07\x1b]133;D;3\x07\
\x1b]133;A\x07$ \x1b]133;B\x07\x1b]133;C\x07out\r\n\
\x1b]1337;llmsh;p=ls%20-l%20%22%24%C3%A9%22\x07\x1b]133;D;0\x07\x1b]133;A\x07$ ";

    fn check(t: &BlockTracker) {
        let b: Vec<_> = t.blocks().last(10).collect();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].output, "hi\nline\ttwo\n");
        assert_eq!(b[0].exit_code, Some(3));
        assert_eq!(b[0].cwd, "/tmp/a b");
        assert_eq!(b[1].output, "out\n");
        assert_eq!(b[1].exit_code, Some(0));
        assert_eq!(b[1].command, "ls -l \"$é\"");
        assert_eq!(t.blocks().last(1).count(), 1);
    }

    #[test]
    fn whole_stream() {
        let mut t = BlockTracker::default();
        t.feed(STREAM);
        check(&t);
        assert_eq!(t.blocks().last(10).next().unwrap().command, "echo hi; x");
    }

    #[test]
    fn split_everywhere() {
        let mut t = BlockTracker::default();
        for c in STREAM.chunks(1) {
            t.feed(c);
        }
        check(&t);
    }

    #[test]
    fn empty_command_makes_no_block() {
        let mut t = BlockTracker::default();
        t.feed(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        assert!(t.blocks().is_empty());
    }

    #[test]
    fn output_capped_head_and_tail() {
        let mut t = BlockTracker::default();
        t.feed(b"\x1b]133;C\x07HEAD");
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..16 {
            t.feed(&chunk);
        }
        t.feed(b"TAIL\x1b]133;D;0\x07");
        let o = &t.blocks().last(1).next().unwrap().output;
        assert!(o.len() < 300 * 1024 && o.starts_with("HEAD") && o.ends_with("TAIL"));
        assert!(o.contains("bytes omitted"));
    }

    #[test]
    fn unterminated_osc_does_not_swallow_output() {
        let mut t = BlockTracker::default();
        t.feed(b"\x1b]133;C\x07\x1b]x");
        t.feed(&vec![b'a'; 200 * 1024]);
        t.feed(b"visible\n\x1b]133;D;0\x07");
        assert!(t
            .blocks()
            .last(1)
            .next()
            .unwrap()
            .output
            .contains("visible"));
    }

    #[test]
    fn ring_is_bounded() {
        let mut t = BlockTracker::default();
        for _ in 0..150 {
            t.feed(b"\x1b]133;C\x07a\x1b]133;D;0\x07");
        }
        assert_eq!(t.blocks().len(), MAX_BLOCKS);
    }
}
