//! What the model gets to see: recent blocks (trimmed to a token budget),
//! cwd, OS, bash version, git state.

use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::blocks::{Block, BlockTracker};

const GIT_TIMEOUT: Duration = Duration::from_millis(200);
pub const CHARS_PER_TOKEN: usize = 4;

/// Token budget for the context: min(window/4, 6k).
pub fn budget_tokens(window: Option<u64>) -> usize {
    window.map_or(6000, |w| (w / 4).min(6000) as usize).max(500)
}

/// Keep head (1/3) and tail (2/3) of `s` within `max` chars.
pub fn trim_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_owned();
    }
    let head = max / 3;
    let tail = max - head;
    let h: String = s.chars().take(head).collect();
    let t: String = s.chars().skip(n - tail).collect();
    format!("{h}\n[... {} chars omitted ...]\n{t}", n - max)
}

fn os_info() -> &'static str {
    static OS: OnceLock<String> = OnceLock::new();
    OS.get_or_init(|| {
        let pretty = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                    .map(|v| v.trim_matches('"').to_owned())
            });
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        format!(
            "{} {kernel} ({})",
            pretty.unwrap_or_else(|| std::env::consts::OS.to_owned()),
            std::env::consts::ARCH
        )
    })
}

fn bash_version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        Command::new("bash")
            .args(["-c", "echo $BASH_VERSION"])
            .stdin(Stdio::null())
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    })
}

/// stdout of `git <args>` in `cwd`, or None if it fails or takes > 200 ms.
fn git(args: &[&str], cwd: &str) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    let cwd = cwd.to_owned();
    std::thread::spawn(move || {
        let r = Command::new("git")
            .args(&args)
            .current_dir(if cwd.is_empty() { "." } else { &cwd })
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        let _ = tx.send(r);
    });
    rx.recv_timeout(GIT_TIMEOUT).ok().flatten()
}

fn fmt_block(i: usize, b: &Block, out_budget: usize) -> String {
    let code = b.exit_code.map_or("?".into(), |c| c.to_string());
    let out = trim_middle(b.output.trim_end(), out_budget);
    format!(
        "--- command {i}: {}\nexit: {code}, duration: {:.1}s\noutput:\n{out}\n",
        b.command,
        b.duration.as_secs_f32()
    )
}

/// Build the context text for a question about the last `n` blocks.
pub fn build(tracker: &Arc<Mutex<BlockTracker>>, n: usize, window: Option<u64>) -> String {
    let (blocks, cwd): (Vec<Block>, String) = match tracker.lock() {
        Ok(t) => (
            t.blocks().last(n.max(1)).cloned().collect(),
            t.cwd().to_owned(),
        ),
        Err(_) => (Vec::new(), String::new()),
    };
    let cwd_for_git = cwd.clone();
    let git_status = std::thread::spawn(move || git(&["status", "-s"], &cwd_for_git));
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"], &cwd);
    let status = git_status.join().ok().flatten();
    let mut s = format!("cwd: {cwd}\nos: {}\nbash: {}\n", os_info(), bash_version());
    if let Some(b) = branch {
        s.push_str(&format!("git branch: {}\n", b.trim()));
        let st: Vec<&str> = status.as_deref().unwrap_or("").lines().take(20).collect();
        if !st.is_empty() {
            s.push_str("git status:\n");
            s.push_str(&st.join("\n"));
            s.push('\n');
        }
    }
    let total = budget_tokens(window) * CHARS_PER_TOKEN;
    let per = (total / blocks.len().max(1)).max(400);
    for (i, b) in blocks.iter().enumerate() {
        s.push_str(&fmt_block(i + 1, b, per));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_head_and_tail() {
        let s: String = ('a'..='z').cycle().take(1000).collect();
        let t = trim_middle(&s, 90);
        assert!(t.starts_with(&s[..30]) && t.ends_with(&s[940..]));
        assert!(t.contains("910 chars omitted"));
        assert_eq!(trim_middle("short", 90), "short");
    }

    #[test]
    fn budget() {
        assert_eq!(budget_tokens(None), 6000);
        assert_eq!(budget_tokens(Some(400_000)), 6000);
        assert_eq!(budget_tokens(Some(8192)), 2048);
    }
}
