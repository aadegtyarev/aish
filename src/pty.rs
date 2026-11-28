//! PTY creation and the byte pumps of the proxy.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use nix::libc;
use nix::pty::openpty;

use crate::blocks::BlockTracker;
use crate::editor::ctl::{self, Input, Shared, ESC_WAIT, QUIET};
use crate::editor::overlay;
use crate::osc::Filter;

/// Spawn `cmd` on a fresh pty as session leader with the pty as controlling
/// terminal. Returns the child and the master side.
pub fn spawn(mut cmd: Command, ws: &libc::winsize) -> Result<(Child, File)> {
    let pty = openpty(Some(ws), None).context("openpty")?;
    for fd in [&pty.master, &pty.slave] {
        fcntl(fd.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    }
    cmd.stdin(Stdio::from(pty.slave.try_clone()?))
        .stdout(Stdio::from(pty.slave.try_clone()?))
        .stderr(Stdio::from(pty.slave));
    // SAFETY: only async-signal-safe calls (setsid, ioctl) between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().context("spawn shell")?;
    drop(cmd); // closes our copies of the slave so master sees EIO on exit
    Ok((child, File::from(pty.master)))
}

/// Wait up to `d` for `f` to become readable (or hang up).
fn readable(f: &File, d: std::time::Duration) -> bool {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    let ms = PollTimeout::try_from(d).unwrap_or(PollTimeout::MAX);
    let mut fds = [PollFd::new(f.as_fd(), PollFlags::POLLIN)];
    !matches!(poll(&mut fds, ms), Ok(0))
}

/// Wait for stdin or the wake pipe; (stdin ready, wake ready). `None`
/// timeout waits forever; a timeout returns (false, false).
fn wait_input(stdin: &File, wake: &File, timeout: Option<std::time::Duration>) -> (bool, bool) {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    let ms = match timeout {
        Some(d) => PollTimeout::try_from(d).unwrap_or(PollTimeout::MAX),
        None => PollTimeout::NONE,
    };
    let mut fds = [
        PollFd::new(stdin.as_fd(), PollFlags::POLLIN),
        PollFd::new(wake.as_fd(), PollFlags::POLLIN),
    ];
    match poll(&mut fds, ms) {
        Ok(n) if n > 0 => {
            let ready = |f: &PollFd| f.revents().is_some_and(|r| !r.is_empty());
            (ready(&fds[0]), ready(&fds[1]))
        }
        Ok(_) => (false, false),
        Err(_) => (true, false), // EINTR etc.: let read() sort it out
    }
}

/// Bytes the controller wants typed into bash (written without any lock held).
fn write_out(master: &mut File, bytes: &[u8]) -> bool {
    bytes.is_empty() || master.write_all(bytes).is_ok()
}

/// Terminal -> pty. Raw blocking read/write, no buffering, no allocation.
/// With a controller, bytes are routed through the line editor while it owns
/// the prompt and forwarded untouched otherwise; `wake` is the read end of the
/// pipe plugins use to announce late results.
pub fn pump_input(mut stdin: File, mut master: File, is_tty: bool, ed: Option<(Shared, File)>) {
    // a little room over a read for a key put back after a failed overlay open
    let mut buf = [0u8; 4096 + 64];
    let mut pending = false;
    let gate = ed.as_ref().map(|(c, _)| ctl::lock(c).gate());
    // bytes to run through the normal path again (the overlay could not open)
    let mut replay: Option<Vec<u8>> = None;
    loop {
        if let (Some((c, wake)), None) = (&ed, &replay) {
            let timeout = pending.then_some(ESC_WAIT);
            let (inp, woke) = wait_input(&stdin, wake, timeout);
            if woke {
                let mut sink = [0u8; 64];
                let _ = (&*wake).read(&mut sink);
                let ((), out) = ctl::with(c, (), |g| g.poll_plugins());
                if !write_out(&mut master, &out) {
                    return;
                }
            }
            if !inp {
                if pending && !woke {
                    let ((), out) = ctl::with(c, (), |g| g.flush_keys());
                    pending = false;
                    if !write_out(&mut master, &out) {
                        return;
                    }
                }
                continue;
            }
        }
        let read = match replay.take() {
            Some(r) => {
                let n = r.len().min(buf.len());
                buf[..n].copy_from_slice(&r[..n]);
                Ok(n)
            }
            None => stdin.read(&mut buf[..4096]),
        };
        match read {
            Ok(0) => {
                if !is_tty {
                    let _ = master.write_all(&[0x04]); // piped input: ^D at the end
                }
                return;
            }
            Ok(n) => match &ed {
                None => {
                    if master.write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
                Some((c, _)) => {
                    let (r, out) = ctl::with(c, Input::Forward(n), |g| g.on_input(&mut buf, n));
                    let ok = match r {
                        Input::Forward(l) => {
                            pending = false;
                            write_out(&mut master, &buf[..l])
                        }
                        Input::ForwardOwned(v) => {
                            pending = false;
                            write_out(&mut master, &v)
                        }
                        Input::Handled(p) => {
                            pending = p;
                            true
                        }
                        Input::Ask(req) => {
                            pending = false;
                            let (Some(g), Some((_, wake))) = (&gate, &ed) else {
                                continue;
                            };
                            if !write_out(&mut master, &req.before) {
                                return;
                            }
                            // The output thread parks; the state is read after it did.
                            g.open();
                            let (setup, ob) = ctl::with(c, None, |g| g.overlay_setup());
                            let mut ok = write_out(&mut master, &ob);
                            ok &= match setup {
                                Some(s) => overlay::run(s, &req, &stdin, wake, &mut master),
                                None => {
                                    g.close();
                                    // not the moment after all: the key and what
                                    // followed take the normal route again
                                    replay = Some([&req.key[..], &req.after[..]].concat());
                                    true
                                }
                            };
                            ok
                        }
                    };
                    if !ok || !write_out(&mut master, &out) {
                        return;
                    }
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Pty -> terminal. Each chunk is forwarded first (minus private OSCs), and
/// only then fed to the block tracker, so parsing never delays display.
pub fn pump_output(
    mut master: File,
    mut stdout: File,
    tracker: Arc<Mutex<BlockTracker>>,
    ed: Option<Shared>,
) {
    let mut armed = false;
    let mut seen_blocks = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    let mut out = Vec::with_capacity(buf.len());
    let mut filter = Filter::default();
    let gate = ed.as_ref().map(|c| ctl::lock(c).gate());
    loop {
        if let (true, Some(c)) = (armed, &ed) {
            if !readable(&master, QUIET) {
                let ((), ob) = ctl::with(c, (), |g| g.on_quiet());
                armed = false;
                let _ = write_out(&mut master, &ob);
                continue;
            }
        }
        let n = match master.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return, // EIO: shell gone
        };
        // While the overlay is open the chunk waits here (the app blocks on
        // its next write); nothing is lost.
        let _busy = gate.as_ref().map(|g| g.enter());
        out.clear();
        filter.filter(&buf[..n], &mut out);
        if let Some(c) = &ed {
            ctl::with(c, (), |g| g.job_filter(&mut out));
        }
        match &ed {
            None => {
                if stdout.write_all(&out).is_err() {
                    return;
                }
            }
            Some(c) => {
                let (handled, ob) =
                    ctl::with(c, None, |g| g.in_prompt().then(|| g.output_in_prompt(&out)));
                if let Some(a) = handled {
                    armed = a;
                    let _ = write_out(&mut master, &ob);
                } else {
                    if stdout.write_all(&out).is_err() {
                        return;
                    }
                    let (a, ob) = ctl::with(c, false, |g| g.after_passthrough_output(&out));
                    armed = a;
                    let _ = write_out(&mut master, &ob);
                }
            }
        }
        let mut jobs_news = None;
        let mut empty_prompt = false;
        let finished = tracker.lock().ok().and_then(|mut t| {
            t.feed(&buf[..n]);
            jobs_news = t.take_job_update();
            empty_prompt = t.take_empty_prompt();
            let total = t.blocks().total();
            if total != seen_blocks {
                seen_blocks = total;
                t.blocks().last(1).next().cloned().map(|b| (total, b))
            } else {
                None
            }
        });
        if let (Some((rows, ends)), Some(c)) = (jobs_news, &ed) {
            let ((), ob) = ctl::with(c, (), |g| g.on_jobs(rows, ends));
            let _ = write_out(&mut master, &ob);
        }
        if let (Some((total, b)), Some(c)) = (finished, &ed) {
            ctl::with(c, (), |g| g.on_block(total, &b));
        }
        if let (true, Some(c)) = (empty_prompt, &ed) {
            ctl::with(c, (), |g| g.on_empty_prompt());
        }
    }
}

pub fn dup_std(fd: &impl AsFd) -> Result<File> {
    let owned: OwnedFd = fd.as_fd().try_clone_to_owned()?;
    Ok(File::from(owned))
}

/// A non-blocking stdin/stdout (inherited file description) would make the
/// pumps fail with EAGAIN; force blocking mode.
pub fn clear_nonblock(fd: std::os::fd::RawFd) {
    use nix::fcntl::OFlag;
    if let Ok(f) = fcntl(fd, FcntlArg::F_GETFL) {
        let f = OFlag::from_bits_truncate(f);
        if f.contains(OFlag::O_NONBLOCK) {
            let _ = fcntl(fd, FcntlArg::F_SETFL(f - OFlag::O_NONBLOCK));
        }
    }
}
