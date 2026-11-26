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

/// Terminal -> pty. Raw blocking read/write, no buffering, no allocation.
pub fn pump_input(mut stdin: File, mut master: File, is_tty: bool) {
    let mut buf = [0u8; 4096];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => {
                if !is_tty {
                    let _ = master.write_all(&[0x04]); // piped input: ^D at the end
                }
                return;
            }
            Ok(n) => {
                if master.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Pty -> terminal. Each chunk is forwarded first (minus private OSCs), and
/// only then fed to the block tracker, so parsing never delays display.
pub fn pump_output(mut master: File, mut stdout: File, tracker: Arc<Mutex<BlockTracker>>) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut out = Vec::with_capacity(buf.len());
    let mut filter = Filter::default();
    loop {
        let n = match master.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return, // EIO: shell gone
        };
        out.clear();
        filter.filter(&buf[..n], &mut out);
        if stdout.write_all(&out).is_err() {
            return;
        }
        if let Ok(mut t) = tracker.lock() {
            t.feed(&buf[..n]);
        }
    }
}

pub fn dup_std(fd: &impl AsFd) -> Result<File> {
    let owned: OwnedFd = fd.as_fd().try_clone_to_owned()?;
    Ok(File::from(owned))
}
