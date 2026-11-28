//! Host terminal: raw mode, window size, SIGWINCH.

use std::io::IsTerminal;
use std::os::fd::RawFd;

use nix::libc;
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg, Termios};

/// The terminal's settings before raw mode, for the panic hook and the signal thread.
static COOKED: std::sync::Mutex<Option<Termios>> = std::sync::Mutex::new(None);

fn restore() {
    if let Some(t) = COOKED.lock().ok().as_deref().and_then(Option::as_ref) {
        let _ = tcsetattr(std::io::stdin(), SetArg::TCSANOW, t);
    }
}

thread_local! {
    /// Set while a controller call runs under `catch_unwind`: the panic hook must
    /// not touch the terminal then, the session continues.
    pub static IN_GUARD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

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
        let cooked = tcgetattr(std::io::stdin()).ok()?;
        let mut raw = cooked.clone();
        cfmakeraw(&mut raw);
        tcsetattr(std::io::stdin(), SetArg::TCSANOW, &raw).ok()?;
        if let Ok(mut c) = COOKED.lock() {
            c.get_or_insert(cooked);
        }
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if IN_GUARD.with(|g| g.get()) {
                tracing::error!("caught panic: {info}");
                return;
            }
            restore();
            prev(info);
        }));
        Some(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        restore();
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

/// Signals handled by aish itself. Register before spawning the shell so none
/// is lost, then call [`Signals::watch`] once the child exists.
pub struct Signals(signal_hook::iterator::Signals);

impl Signals {
    pub fn register() -> std::io::Result<Signals> {
        use signal_hook::consts::*;
        signal_hook::iterator::Signals::new([SIGWINCH, SIGTERM, SIGQUIT, SIGHUP]).map(Signals)
    }

    /// `on_winch` runs at once (closing the race with startup) and on every
    /// WINCH; TERM/QUIT/HUP run `on_exit`, restore the terminal, hang up the
    /// shell and exit.
    pub fn watch(
        mut self,
        child_pid: u32,
        on_winch: impl Fn() + Send + 'static,
        on_exit: impl Fn() + Send + 'static,
    ) {
        std::thread::spawn(move || {
            use signal_hook::consts::*;
            on_winch();
            for sig in self.0.forever() {
                if sig == SIGWINCH {
                    on_winch();
                    continue;
                }
                on_exit();
                restore();
                // SAFETY: plain kill(2) on our own child.
                unsafe { libc::kill(child_pid as i32, SIGHUP) };
                std::process::exit(128 + sig);
            }
        });
    }
}
