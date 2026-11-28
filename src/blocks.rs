//! Command blocks built from OSC 133 marks in the PTY output stream.
//!
//! Marks: A prompt start, B input start, C output start, D;<exit> finished.
//! Private `1337;aish;{t,b}=<cmd>` (plain / base64) arrives just before D.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

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
    pub duration: Duration,
}

/// A live background job as reported by the shell (`jobs -l`).
#[derive(Debug, Clone, PartialEq)]
pub struct JobRow {
    pub id: u32,
    pub pid: u32,
    /// `Running` or `Stopped`.
    pub state: String,
    pub cmd: String,
}

/// A background job that finished: `status` is the exit code, or `sig:<text>`.
#[derive(Debug, Clone, PartialEq)]
pub struct JobEnd {
    pub id: u32,
    pub pid: u32,
    pub status: String,
    pub cmd: String,
}

/// Rows of the private `jobs=` / `jobend=` payload: `a\tb\tc\td` per line.
fn job_fields(p: &[u8]) -> Vec<[String; 4]> {
    percent_decode(&String::from_utf8_lossy(p))
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(4, '\t').map(str::to_owned);
            Some([f.next()?, f.next()?, f.next()?, f.next()?])
        })
        .collect()
}

/// Ring buffer of the most recent blocks.
#[derive(Debug, Default)]
pub struct Blocks {
    ring: VecDeque<Block>,
    total: u64,
}

impl Blocks {
    fn push(&mut self, b: Block) {
        self.total += 1;
        if self.ring.len() == MAX_BLOCKS {
            self.ring.pop_front();
        }
        self.ring.push_back(b);
    }

    /// The last `n` blocks, oldest first.
    pub fn last(&self, n: usize) -> impl Iterator<Item = &Block> {
        self.ring.iter().skip(self.ring.len().saturating_sub(n))
    }

    /// All blocks, newest first.
    pub fn newest_first(&self) -> impl Iterator<Item = &Block> {
        self.ring.iter().rev()
    }

    /// Number of blocks ever finished (monotonic, unlike `len`).
    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }
}

/// Output accumulator that keeps the first HEAD_CAP and last TAIL_CAP bytes.
#[derive(Default, Clone)]
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
    cwd: String,
    out: Capped,
}

#[derive(Default)]
struct Collector {
    /// When set, OSC 133 marks must carry `aish=<nonce>` (foreign shells' marks are ignored).
    nonce: Option<String>,
    path: Option<String>,
    path_version: u64,
    jobs: Vec<JobRow>,
    jobs_changed: bool,
    job_ends: Vec<JobEnd>,
    cwd: String,
    /// A prompt came back with no command started since the last one.
    empty_prompt: bool,
    pending_cmd: Option<String>,
    run: Option<Run>,
    blocks: Blocks,
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
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
                duration: run.started.elapsed(),
            };
            tracing::debug!(cmd = %block.command, exit = ?block.exit_code, ms = block.duration.as_millis() as u64, bytes = block.output.len(), "block");
            self.blocks.push(block);
        }
    }
}

impl Collector {
    fn tagged(&self, params: &[&[u8]]) -> bool {
        match &self.nonce {
            None => true,
            Some(n) => params.iter().any(|p| *p == format!("aish={n}").as_bytes()),
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
            [b"133", _, rest @ ..] if !self.tagged(rest) => {}
            [b"133", mark, rest @ ..] => match mark.first() {
                Some(b'C') => {
                    self.run = Some(Run {
                        started: Instant::now(),
                        cwd: self.cwd.clone(),
                        out: Capped::default(),
                    });
                }
                Some(b'D') => {
                    let exit = rest
                        .first()
                        .and_then(|p| std::str::from_utf8(p).ok()?.parse().ok());
                    self.empty_prompt |= self.run.is_none();
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
            [b"1337", b"aish", payload @ ..] => {
                // vte splits on ';', and plain commands may contain it.
                debug_assert!(PRIVATE_PREFIX.ends_with(b"aish;"));
                let payload = payload.join(&b';');
                if let Some(p) = payload.strip_prefix(b"jobs=") {
                    self.jobs = job_fields(p)
                        .into_iter()
                        .filter_map(|[id, pid, state, cmd]| {
                            Some(JobRow {
                                id: id.parse().ok()?,
                                pid: pid.parse().ok()?,
                                state,
                                cmd,
                            })
                        })
                        .collect();
                    self.jobs_changed = true;
                    return;
                }
                if let Some(p) = payload.strip_prefix(b"jobend=") {
                    for [id, pid, status, cmd] in job_fields(p) {
                        if let (Ok(id), Ok(pid)) = (id.parse(), pid.parse()) {
                            self.job_ends.push(JobEnd {
                                id,
                                pid,
                                status,
                                cmd,
                            });
                        }
                    }
                    self.jobs_changed = true;
                    return;
                }
                if let Some(p) = payload.strip_prefix(b"path=") {
                    self.path = Some(percent_decode(&String::from_utf8_lossy(p)));
                    self.path_version += 1;
                    return;
                }
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

    /// Plain-text output of the command that is running now (None when no
    /// command has started since the last prompt).
    pub fn running_output(&self) -> Option<String> {
        self.state.run.as_ref().map(|r| r.out.clone().finish())
    }

    /// Did a prompt come back since the last call without a command having
    /// started (a syntax error, an unfinished quote, an empty line)?
    pub fn take_empty_prompt(&mut self) -> bool {
        std::mem::take(&mut self.state.empty_prompt)
    }

    /// The job table and finished jobs, if the shell reported news since the last call.
    pub fn take_job_update(&mut self) -> Option<(Vec<JobRow>, Vec<JobEnd>)> {
        if !std::mem::take(&mut self.state.jobs_changed) {
            return None;
        }
        Some((
            self.state.jobs.clone(),
            std::mem::take(&mut self.state.job_ends),
        ))
    }

    /// PATH of the shell (reported by the integration whenever it changes)
    /// and a counter that increases with every report.
    pub fn path(&self) -> (Option<&str>, u64) {
        (self.state.path.as_deref(), self.state.path_version)
    }

    /// Only accept OSC 133 marks tagged with this session nonce.
    pub fn with_nonce(nonce: String) -> Self {
        let mut t = BlockTracker::default();
        t.state.nonce = Some(nonce);
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &[u8] = b"\x1b]7;file://h/tmp/a%20b\x07\x1b]133;A\x07$ \x1b]133;B\x07\
\x1b]133;C\x07\x1b[1mhi\x1b[0m\r\nline\ttwo\r\n\
\x1b]1337;aish;p=echo%20hi;%20x\x07\x1b]133;D;3\x07\
\x1b]133;A\x07$ \x1b]133;B\x07\x1b]133;C\x07out\r\n\
\x1b]1337;aish;p=ls%20-l%20%22%24%C3%A9%22\x07\x1b]133;D;0\x07\x1b]133;A\x07$ ";

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
    fn a_prompt_without_a_started_command_is_noticed() {
        let mut t = BlockTracker::default();
        t.feed(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        assert!(t.take_empty_prompt());
        assert!(!t.take_empty_prompt());
        t.feed(b"\x1b]133;C\x07x\x1b]133;D;0\x07");
        assert!(!t.take_empty_prompt());
    }

    #[test]
    fn empty_command_makes_no_block() {
        let mut t = BlockTracker::default();
        t.feed(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        assert_eq!(t.blocks().len(), 0);
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
    fn running_output_is_visible_before_the_block_ends() {
        let mut t = BlockTracker::default();
        assert_eq!(t.running_output(), None);
        t.feed(b"\x1b]133;C\x07\x1b[1mone\x1b[0m\r\ntwo");
        assert_eq!(t.running_output().as_deref(), Some("one\ntwo"));
        t.feed(b"\r\n\x1b]133;D;0\x07");
        assert_eq!(t.running_output(), None);
    }

    #[test]
    fn ring_is_bounded() {
        let mut t = BlockTracker::default();
        for _ in 0..150 {
            t.feed(b"\x1b]133;C\x07a\x1b]133;D;0\x07");
        }
        assert_eq!(t.blocks().len(), MAX_BLOCKS);
    }

    #[test]
    fn nonce_filters_foreign_marks_and_path_is_tracked() {
        let mut t = BlockTracker::with_nonce("n1".into());
        // a foreign shell (ssh) inside the command: no block, no confusion
        t.feed(
            b"\x1b]133;C;aish=n1\x07out\x1b]133;A\x07$ \x1b]133;D;5\x07\x1b]133;D;0;aish=n1\x07",
        );
        let b: Vec<_> = t.blocks().last(10).collect();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].exit_code, Some(0));
        t.feed(b"\x1b]1337;aish;path=/a:/b%20c\x07");
        assert_eq!(t.path(), (Some("/a:/b c"), 1));
    }
}
