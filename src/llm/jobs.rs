//! Background jobs of the shell, as far as llmsh knows them (reported by the
//! rc hook, see `rc.bash`), the logs of the ones the agent started, and the
//! one-line notice when a job finishes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::blocks::{JobEnd, JobRow};

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u32,
    pub pid: u32,
    pub command: String,
    /// `Running`, `Stopped`.
    pub state: String,
    /// When llmsh first saw the job.
    pub started: Instant,
}

/// A finished job, ready to be shown.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub id: u32,
    pub cmd: String,
    pub status: String,
    pub secs: Option<u64>,
}

#[derive(Debug, Default)]
pub struct Jobs {
    pub list: Vec<Job>,
    /// Logs of agent-started jobs by job number (`<command> &>log &`).
    pub links: HashMap<u32, PathBuf>,
    pub seq: u64,
    /// Jobs that just left the table (their end report may follow).
    gone: Vec<(Job, Instant)>,
}

impl Jobs {
    /// Take the shell's news: the live table and the jobs that ended.
    pub fn apply(&mut self, rows: &[JobRow], ends: &[JobEnd]) -> Vec<Finished> {
        let fin = ends
            .iter()
            .filter(|e| e.status != "?")
            .map(|e| Finished {
                id: e.id,
                cmd: e.cmd.clone(),
                status: e.status.clone(),
                secs: self
                    .list
                    .iter()
                    .chain(self.gone.iter().map(|(j, _)| j))
                    .find(|j| j.id == e.id && j.pid == e.pid)
                    .map(|j| j.started.elapsed().as_secs()),
            })
            .collect();
        let old = std::mem::take(&mut self.list);
        self.list = rows
            .iter()
            .map(|r| Job {
                id: r.id,
                pid: r.pid,
                command: r.cmd.clone(),
                state: r.state.clone(),
                started: old
                    .iter()
                    .find(|j| j.id == r.id && j.pid == r.pid)
                    .map_or_else(Instant::now, |j| j.started),
            })
            .collect();
        self.gone
            .retain(|(_, t)| t.elapsed() < Duration::from_secs(10));
        for j in old {
            if !self.list.iter().any(|n| n.id == j.id && n.pid == j.pid) {
                self.gone.push((j, Instant::now()));
            }
        }
        let live: Vec<u32> = self.list.iter().map(|j| j.id).collect();
        // a link made just before the table reported the job must survive
        self.links
            .retain(|id, _| live.contains(id) || ends.iter().all(|e| e.id != *id));
        fin
    }

    /// One line per job for the model.
    pub fn describe(&self) -> String {
        if self.list.is_empty() {
            return "no background jobs".into();
        }
        self.list
            .iter()
            .map(|j| {
                format!(
                    "[{}] {} {} (running {}s){}",
                    j.id,
                    j.state,
                    j.command,
                    j.started.elapsed().as_secs(),
                    if self.links.contains_key(&j.id) {
                        " — log available (job_output)"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn link(&mut self, id: u32, log: PathBuf) {
        self.links.insert(id, log);
    }

    pub fn log_of(&self, id: u32) -> Option<PathBuf> {
        self.links.get(&id).cloned()
    }

    /// Pids that are no longer there (the poller pokes the shell then).
    pub fn any_dead(&self) -> bool {
        self.list.iter().any(|j| !pid_alive(j.pid))
    }
}

fn pid_alive(pid: u32) -> bool {
    let p = nix::unistd::Pid::from_raw(pid as i32);
    if nix::sys::signal::kill(p, None).is_err_and(|e| e == nix::errno::Errno::ESRCH) {
        return false;
    }
    // a zombie is dead too (bash reaps it at once, so this is brief)
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')
                .map(|(_, r)| r.trim_start().starts_with('Z'))
        })
        != Some(true)
}

/// `41s`, `2m13s`, `1h02m`.
pub fn fmt_dur(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// `(ok, "[1] cargo build · 2m13s")`, the command cut to fit one line of
/// `cols` columns after the ` ✓ ` mark.
pub fn notice_text(f: &Finished, cols: usize) -> (bool, String) {
    let ok = f.status == "0";
    let mut tail = String::new();
    if let Some(s) = f.secs {
        tail.push_str(&format!(" · {}", fmt_dur(s)));
    }
    if !ok {
        match f.status.strip_prefix("sig:") {
            Some(s) => tail.push_str(&format!(" · {}", s.trim())),
            None => tail.push_str(&format!(" · exit {}", f.status)),
        }
    }
    let head = format!("[{}] ", f.id);
    let room = cols
        .saturating_sub(3 + head.chars().count() + tail.chars().count() + 1)
        .max(8);
    let one: String = f
        .cmd
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cmd = if one.chars().count() > room {
        let t: String = one.chars().take(room - 1).collect();
        format!("{t}…")
    } else {
        one
    };
    (ok, format!("{head}{cmd}{tail}"))
}

/// Removes bash's own job report lines (`[1]+  Done  cmd`) from a stream of
/// output chunks; the controller uses it while it has asked the shell about a
/// job itself and prints its own line instead.
pub struct LineStripper {
    at_start: bool,
    skipping: bool,
}

impl Default for LineStripper {
    fn default() -> Self {
        LineStripper {
            at_start: true,
            skipping: false,
        }
    }
}

impl LineStripper {
    pub fn strip(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            if self.skipping {
                match data[i..].iter().position(|&b| b == b'\n') {
                    Some(n) => {
                        i += n + 1;
                        self.skipping = false;
                        self.at_start = true;
                    }
                    None => i = data.len(),
                }
                continue;
            }
            if self.at_start && data[i] == b'[' {
                let mut j = i + 1;
                while j < data.len() && data[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 1
                    && data.get(j) == Some(&b']')
                    && matches!(data.get(j + 1), Some(b'+' | b'-' | b' '))
                {
                    self.skipping = true;
                    continue;
                }
            }
            self.at_start = matches!(data[i], b'\n' | b'\r');
            out.push(data[i]);
            i += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(fmt_dur(41), "41s");
        assert_eq!(fmt_dur(133), "2m13s");
        assert_eq!(fmt_dur(60), "1m00s");
        assert_eq!(fmt_dur(3720), "1h02m");
    }

    #[test]
    fn notices() {
        let f = Finished {
            id: 1,
            cmd: "cargo build --release".into(),
            status: "0".into(),
            secs: Some(133),
        };
        assert_eq!(
            notice_text(&f, 80),
            (true, "[1] cargo build --release · 2m13s".into())
        );
        let f = Finished {
            id: 2,
            cmd: "npm test".into(),
            status: "1".into(),
            secs: Some(41),
        };
        assert_eq!(
            notice_text(&f, 80),
            (false, "[2] npm test · 41s · exit 1".into())
        );
        let f = Finished {
            id: 3,
            cmd: "x".repeat(200),
            status: "sig:Terminated".into(),
            secs: None,
        };
        let (ok, t) = notice_text(&f, 40);
        assert!(
            !ok && t.chars().count() < 40 && t.contains('…') && t.ends_with("Terminated"),
            "{t}"
        );
    }

    #[test]
    fn bash_report_lines_are_removed() {
        let d = "\x1b[K\r[2]+  Exit 3               sh -c x\r\n\x1b[?2004hP> ".as_bytes();
        assert_eq!(
            LineStripper::default().strip(d),
            "\x1b[K\r\x1b[?2004hP> ".as_bytes()
        );
        // the line split over two chunks
        let mut s = LineStripper::default();
        assert_eq!(s.strip(b"\r[1]+  Done  sl"), b"\r");
        assert_eq!(s.strip(b"eep 1\r\nE> "), b"E> ");
        // not at a line start, or not a job line: kept
        for k in ["echo [1] x\r\n", "[a] x\r\n", "[1]x\r\n", "ok\r\n[ 1 ]\r\n"] {
            assert_eq!(
                LineStripper::default().strip(k.as_bytes()),
                k.as_bytes(),
                "{k}"
            );
        }
    }

    #[test]
    fn table_updates_keep_start_times_and_report_ends() {
        let mut j = Jobs::default();
        let row = |id, pid| JobRow {
            id,
            pid,
            state: "Running".into(),
            cmd: "sleep 9".into(),
        };
        assert!(j.apply(&[row(1, 10)], &[]).is_empty());
        let t0 = j.list[0].started;
        j.apply(&[row(1, 10), row(2, 11)], &[]);
        assert_eq!(j.list[0].started, t0);
        let end = JobEnd {
            id: 1,
            pid: 10,
            status: "3".into(),
            cmd: "sleep 9".into(),
        };
        let fin = j.apply(&[row(2, 11)], &[end]);
        assert_eq!(fin.len(), 1);
        assert_eq!(
            (fin[0].id, fin[0].status.as_str(), fin[0].secs),
            (1, "3", Some(0))
        );
        assert_eq!(j.list.len(), 1);
        assert!(j.describe().starts_with("[2] Running sleep 9"));
    }
}
