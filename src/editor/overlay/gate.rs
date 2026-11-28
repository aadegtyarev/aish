//! Parking the output thread while the overlay is open, and the terminal
//! state it has to give back.

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

// ---------------------------------------------------------------------------

/// Parks the output thread while the overlay is open. The output thread
/// holds a [`Busy`] while it handles a chunk; opening waits until it is idle,
/// so the screen and mode state are exact and nothing is drawn halfway.
/// While the overlay is closed `enter` and `Busy::drop` are two atomic ops:
/// no lock, no syscall per output chunk.
#[derive(Default)]
pub struct Gate {
    open: AtomicBool,
    busy: AtomicBool,
    m: Mutex<()>,
    cv: Condvar,
    /// The terminal was resized while the overlay is open.
    pub resized: AtomicBool,
    /// The shell ended: close the overlay and let the output through.
    pub(super) closing: AtomicBool,
    waker: std::sync::OnceLock<Arc<File>>,
    /// Bytes that put the terminal back; taken by whoever leaves first.
    restore: Mutex<Option<Vec<u8>>>,
}

pub struct Busy<'a>(&'a Gate);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
        if self.0.open.load(Ordering::SeqCst) {
            let _g = self.0.lock();
            self.0.cv.notify_all();
        }
    }
}

const TICK: Duration = Duration::from_millis(10);

impl Gate {
    fn lock(&self) -> MutexGuard<'_, ()> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The pipe that wakes the input thread (set once by the controller).
    pub fn set_waker(&self, w: Arc<File>) {
        let _ = self.waker.set(w);
    }

    fn wake(&self) {
        if let Some(w) = self.waker.get() {
            let _ = (&**w).write(&[1]);
        }
    }

    /// Output thread: wait while the overlay is open, then mark busy.
    pub fn enter(&self) -> Busy<'_> {
        loop {
            self.busy.store(true, Ordering::SeqCst);
            if !self.open.load(Ordering::SeqCst) {
                return Busy(self);
            }
            // either we see `open` or `open()` sees us busy
            self.busy.store(false, Ordering::SeqCst);
            let mut g = self.lock();
            self.cv.notify_all();
            while self.open.load(Ordering::SeqCst) {
                g = self
                    .cv
                    .wait_timeout(g, TICK)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
    }

    /// Input thread: stop the output thread (call without the controller lock).
    pub fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        let mut g = self.lock();
        while self.busy.load(Ordering::SeqCst) {
            g = self
                .cv
                .wait_timeout(g, TICK)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    pub fn close(&self) {
        self.open.store(false, Ordering::SeqCst);
        let _g = self.lock();
        self.cv.notify_all();
    }

    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::SeqCst)
    }

    /// The shell has exited: an open overlay closes by itself.
    pub fn child_gone(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.wake();
    }

    pub(super) fn set_restore(&self, b: Vec<u8>) {
        *self.restore.lock().unwrap_or_else(|e| e.into_inner()) = Some(b);
    }

    pub(super) fn take_restore(&self) -> Option<Vec<u8>> {
        self.restore
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// The process is going away with the overlay open: give the screen back.
    pub fn emergency_restore(&self) {
        if let Some(b) = self.take_restore() {
            // SAFETY: plain write(2) of a live buffer to stdout.
            unsafe { nix::libc::write(1, b.as_ptr().cast(), b.len()) };
        }
    }
}

/// What the program had set up on the terminal, as the scanner saw it.
#[derive(Clone, Copy, Default)]
pub struct Modes {
    pub alt: bool,
    pub bracketed: bool,
    /// Bit i = `MOUSE_MODES[i]` is on.
    pub mouse: u8,
    pub cursor_hidden: bool,
}

/// Bytes the overlay sends when it opens: no mouse reporting, no kitty
/// keyboard flags (pushed as 0, popped on leave), full scroll region.
pub(super) fn enter_bytes(alt: bool) -> Vec<u8> {
    let mut v = Vec::new();
    if !alt {
        v.extend_from_slice(b"\x1b[?1049h");
    }
    for m in crate::editor::scan::MOUSE_MODES {
        v.extend_from_slice(format!("\x1b[?{m}l").as_bytes());
    }
    v.extend_from_slice(b"\x1b[>0u\x1b[r\x1b[?2004h\x1b[?25h");
    v
}

/// Bytes that return the terminal to the program: leave our alternate screen
/// (or clear the program's own), and the modes as the program had them.
pub(super) fn restore_bytes(m: Modes) -> Vec<u8> {
    let mut v = b"\x1b[0m\x1b[<1u".to_vec();
    v.extend_from_slice(if m.alt {
        b"\x1b[2J\x1b[H"
    } else {
        b"\x1b[?1049l"
    });
    v.extend_from_slice(if m.bracketed {
        b"\x1b[?2004h"
    } else {
        b"\x1b[?2004l"
    });
    for (i, n) in crate::editor::scan::MOUSE_MODES.iter().enumerate() {
        if m.mouse & (1 << i) != 0 {
            v.extend_from_slice(format!("\x1b[?{n}h").as_bytes());
        }
    }
    v.extend_from_slice(if m.cursor_hidden {
        b"\x1b[?25l"
    } else {
        b"\x1b[?25h"
    });
    v
}

/// Make a program in the alternate screen redraw: its window size changes
/// and changes back (which ssh passes on as a window-change). Runs once the
/// program can write again; the size to restore is read afresh, so a real
/// resize in between is never overwritten.
pub(super) fn nudge(master: &File) {
    let mut ws = crate::term::window_size();
    if ws.ws_row < 2 {
        return;
    }
    ws.ws_row -= 1;
    crate::term::set_window_size(master.as_raw_fd(), &ws);
    std::thread::sleep(Duration::from_millis(40));
    crate::term::set_window_size(master.as_raw_fd(), &crate::term::window_size());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn restore_gives_the_screen_back() {
        let m = Modes::default();
        assert_eq!(
            restore_bytes(m),
            b"\x1b[0m\x1b[<1u\x1b[?1049l\x1b[?2004l\x1b[?25h"
        );
        // what the program had enabled comes back; a hidden cursor stays hidden
        let m = Modes {
            alt: true,
            bracketed: true,
            mouse: 0b101,
            cursor_hidden: true,
        };
        assert_eq!(
            restore_bytes(m),
            b"\x1b[0m\x1b[<1u\x1b[2J\x1b[H\x1b[?2004h\x1b[?1000h\x1b[?1003h\x1b[?25l"
        );
        let e = String::from_utf8(enter_bytes(false)).unwrap();
        assert!(
            e.starts_with("\x1b[?1049h") && e.contains("\x1b[?1006l") && e.contains("\x1b[>0u")
        );
    }

    #[test]
    fn gate_waits_for_the_busy_chunk_and_parks_the_next() {
        let g = Arc::new(Gate::default());
        let busy = g.enter();
        let g2 = g.clone();
        let opener = std::thread::spawn(move || g2.open());
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !opener.is_finished(),
            "open() must wait for the chunk in flight"
        );
        drop(busy);
        opener.join().unwrap();
        assert!(g.is_open());
        let g3 = g.clone();
        let out = std::thread::spawn(move || drop(g3.enter()));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!out.is_finished(), "output stays parked while open");
        g.close();
        out.join().unwrap();
        assert!(!g.is_open());
    }
}
