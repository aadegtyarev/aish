//! Tab completion through a long-lived `bash` sidecar (pipes, no pty) that
//! loads bash-completion and the user's ~/.bashrc and answers requests, plus
//! the pure helpers around it (word splitting, quoting, filename fallback).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const TIMEOUT: Duration = Duration::from_millis(300);
const MAX_RESTARTS: u32 = 5;

use lineedit_wip::completion::{unescape, Candidates, Request, MAX_ITEMS};

fn expand(path: &str, cwd: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            if let Some(h) = std::env::var_os("HOME") {
                return PathBuf::from(h).join(rest.trim_start_matches('/'));
            }
        }
    }
    Path::new(cwd).join(path)
}

/// Filename candidates for the raw word `cur` straight from the filesystem.
pub fn complete_files(cur: &str, cwd: &str) -> Candidates {
    let cur = unescape(cur);
    let (dir, base) = match cur.rfind('/') {
        Some(i) => (&cur[..=i], &cur[i + 1..]),
        None => ("", cur.as_str()),
    };
    let mut items = Vec::new();
    if let Ok(rd) = std::fs::read_dir(expand(if dir.is_empty() { "." } else { dir }, cwd)) {
        for e in rd.flatten().take(MAX_ITEMS * 4) {
            let Some(n) = e.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if n.starts_with(base) && (!n.starts_with('.') || base.starts_with('.')) {
                items.push(format!("{dir}{n}"));
            }
        }
    }
    Candidates {
        filenames: true,
        nospace: false,
        items,
    }
}

/// Sort, dedupe, cap; for file names mark directories with a trailing `/`.
pub fn finish(reply: &mut Candidates, cwd: &str) {
    // bash-completion appends a space to candidates (with `-o nospace`).
    let mut trailing_space = false;
    for it in &mut reply.items {
        if it.ends_with(' ') {
            trailing_space = true;
            it.truncate(it.trim_end_matches(' ').len());
        }
    }
    if trailing_space {
        reply.nospace = false;
    }
    reply.items.retain(|s| !s.is_empty());
    reply.items.sort();
    reply.items.dedup();
    reply.items.truncate(MAX_ITEMS);
    if reply.filenames {
        for it in &mut reply.items {
            if !it.ends_with('/') && expand(it, cwd).is_dir() {
                it.push('/');
            }
        }
    }
}

// ---- sidecar process ----

type Msg = (String, Candidates);

struct Inner {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    rx: Option<Receiver<Msg>>,
    next_id: u64,
    ready: bool,
    restarts: u32,
}

pub struct Sidecar {
    script: PathBuf,
    inner: Mutex<Inner>,
}

fn read_replies(mut out: impl Read, tx: std::sync::mpsc::Sender<Msg>) {
    let mut buf = [0u8; 16384];
    let mut field: Vec<u8> = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    loop {
        let n = match out.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b != 0 {
                field.push(b);
                continue;
            }
            let f = String::from_utf8_lossy(&field).into_owned();
            field.clear();
            if f.is_empty() && fields.len() >= 2 {
                let mut it = std::mem::take(&mut fields).into_iter();
                let id = it.next().unwrap_or_default();
                let flags = it.next().unwrap_or_default();
                let reply = Candidates {
                    filenames: flags.contains('f'),
                    nospace: flags.contains('n'),
                    items: it.collect(),
                };
                if tx.send((id, reply)).is_err() {
                    return;
                }
            } else {
                fields.push(f);
            }
        }
    }
}

impl Sidecar {
    pub fn new() -> Option<Arc<Sidecar>> {
        let script =
            crate::shell::install_file("complete.bash", include_str!("complete.bash")).ok()?;
        let s = Arc::new(Sidecar {
            script,
            inner: Mutex::new(Inner {
                child: None,
                stdin: None,
                rx: None,
                next_id: 1,
                ready: false,
                restarts: 0,
            }),
        });
        {
            let mut g = s.inner.lock().ok()?;
            s.spawn(&mut g);
        }
        Some(s)
    }

    fn spawn(&self, g: &mut Inner) {
        g.child = None;
        g.stdin = None;
        g.ready = false;
        if g.restarts > MAX_RESTARTS {
            return;
        }
        g.restarts += 1;
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new("bash");
        // No controlling terminal: an interactive bash would otherwise grab the
        // foreground process group from llmsh.
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                nix::libc::setsid();
                Ok(())
            });
        }
        let child = cmd
            .arg("--norc")
            .arg("--noprofile")
            .arg("-i")
            .arg(&self.script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_remove("LLMSH")
            .spawn();
        let Ok(mut child) = child else { return };
        let (tx, rx) = channel();
        if let Some(out) = child.stdout.take() {
            std::thread::spawn(move || read_replies(out, tx));
        }
        g.stdin = child.stdin.take();
        g.rx = Some(rx);
        g.child = Some(child);
    }

    fn alive(g: &mut Inner) -> bool {
        match g.child.as_mut().map(Child::try_wait) {
            Some(Ok(None)) => g.stdin.is_some(),
            _ => false,
        }
    }

    /// Send a request and wait for its reply until `deadline`.
    fn roundtrip(
        &self,
        deadline: Instant,
        build: impl FnOnce(&str) -> Vec<u8>,
    ) -> Option<Candidates> {
        // Never block past the deadline, even behind another request (the
        // startup name query may be waiting for the sidecar to boot).
        let mut g = loop {
            match self.inner.try_lock() {
                Ok(g) => break g,
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return None,
            }
        };
        if !Self::alive(&mut g) {
            self.spawn(&mut g);
        }
        g.child.as_ref()?;
        if !g.ready {
            loop {
                let left = deadline.checked_duration_since(Instant::now())?;
                match g.rx.as_ref()?.recv_timeout(left) {
                    Ok((id, _)) if id == "R" => {
                        g.ready = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) => return None,
                    Err(RecvTimeoutError::Disconnected) => {
                        g.child = None;
                        return None;
                    }
                }
            }
        }
        let id = g.next_id.to_string();
        g.next_id += 1;
        let req = build(&id);
        let ok = g
            .stdin
            .as_mut()
            .is_some_and(|s| s.write_all(&req).and_then(|_| s.flush()).is_ok());
        if !ok {
            g.child = None;
            return None;
        }
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            match g.rx.as_ref()?.recv_timeout(left) {
                Ok((rid, reply)) if rid == id => return Some(reply),
                Ok(_) => {} // late reply of an earlier, timed-out request
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => {
                    g.child = None;
                    return None;
                }
            }
        }
    }

    pub fn complete(&self, req: &Request, cwd: &str, timeout: Duration) -> Option<Candidates> {
        let cword = req.words.len() - 1;
        self.roundtrip(Instant::now() + timeout, |id| {
            let mut b = Vec::new();
            let mut field = |s: &str| {
                b.extend_from_slice(s.as_bytes());
                b.push(0);
            };
            field(id);
            field("C");
            field(cwd);
            field(&cword.to_string());
            field(&req.words.len().to_string());
            for w in &req.words {
                field(w);
            }
            field(&req.line);
            b
        })
    }

    /// First item is the sidecar's `$PATH`, the rest alias/builtin/keyword/function names.
    pub fn names(&self, timeout: Duration) -> Option<Candidates> {
        self.roundtrip(Instant::now() + timeout, |id| {
            format!("{id}\0L\0").into_bytes()
        })
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        if let Ok(mut g) = self.inner.lock() {
            if let Some(c) = g.child.as_mut() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_fallback_and_finish() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("docs")).unwrap();
        std::fs::write(d.path().join("data.txt"), "").unwrap();
        std::fs::write(d.path().join(".hid"), "").unwrap();
        let cwd = d.path().to_str().unwrap();
        let mut r = complete_files("d", cwd);
        finish(&mut r, cwd);
        assert_eq!(r.items, ["data.txt", "docs/"]);
        let r = complete_files(".", cwd);
        assert_eq!(r.items, [".hid"]);
    }
}
