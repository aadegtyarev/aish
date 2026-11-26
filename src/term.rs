//! Host terminal: raw mode, window size, SIGWINCH.

use std::io::IsTerminal;
use std::os::fd::{AsRawFd, RawFd};

use nix::libc;

nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, libc::winsize);
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, libc::winsize);

/// Restores cooked mode on drop (and on panic, via the hook installed below).
pub struct RawGuard;

impl RawGuard {
    /// None when stdin is not a terminal (nothing to do) or raw mode failed.
    pub fn enter() -> Option<RawGuard> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        crossterm::terminal::enable_raw_mode().ok()?;
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = crossterm::terminal::disable_raw_mode();
            prev(info);
        }));
        Some(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Size of the host terminal (including pixel size), 80x24 if unknown.
pub fn window_size() -> libc::winsize {
    let ws = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    for fd in [1, 0, 2] {
        let mut probe = ws;
        // SAFETY: valid fd number and a properly sized out-pointer.
        if unsafe { tiocgwinsz(fd, &mut probe) }.is_ok() && probe.ws_col > 0 {
            return probe;
        }
    }
    ws
}

pub fn set_window_size(fd: RawFd, ws: &libc::winsize) {
    // SAFETY: fd is a live pty master owned by the caller.
    let _ = unsafe { tiocswinsz(fd, ws) };
}

/// Propagate SIGWINCH to the pty on a helper thread.
pub fn forward_resizes(master: &impl AsRawFd) -> std::io::Result<()> {
    let fd = master.as_raw_fd();
    let mut sigs = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
    std::thread::spawn(move || {
        for _ in sigs.forever() {
            set_window_size(fd, &window_size());
        }
    });
    Ok(())
}
