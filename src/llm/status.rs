//! The right-edge hint of the input line: `?` normally, `[N] ?` with N
//! background jobs, `⋯` while a ghost suggestion is being fetched (only if it
//! takes longer than 300 ms, and then for at least 200 ms: no flicker).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lineedit_wip::plugins::HintText;

const SHOW_AFTER: Duration = Duration::from_millis(300);
const MIN_SHOWN: Duration = Duration::from_millis(200);

#[derive(Default)]
struct Busy {
    gen: u64,
    active: bool,
    shown_at: Option<Instant>,
}

struct Inner {
    hint: HintText,
    jobs: AtomicUsize,
    busy: Mutex<Busy>,
    after: Duration,
    min: Duration,
}

#[derive(Clone)]
pub struct Status(Arc<Inner>);

impl Status {
    pub fn new(hint: HintText) -> Self {
        Self::with_timing(hint, SHOW_AFTER, MIN_SHOWN)
    }

    pub fn with_timing(hint: HintText, after: Duration, min: Duration) -> Self {
        Status(Arc::new(Inner {
            hint,
            jobs: AtomicUsize::new(0),
            busy: Mutex::default(),
            after,
            min,
        }))
    }

    fn normal(&self) -> String {
        match self.0.jobs.load(Ordering::Relaxed) {
            0 => "?".to_owned(),
            n => format!("[{n}] ?"),
        }
    }

    pub fn set_jobs(&self, n: usize) {
        // under the lock, so a racing begin/end cannot interleave its hint update
        let Ok(b) = self.0.busy.lock() else { return };
        self.0.jobs.store(n, Ordering::Relaxed);
        if b.shown_at.is_none() {
            self.0.hint.set(&self.normal());
        }
    }

    /// A suggestion request started.
    pub fn begin(&self) {
        let gen = {
            let Ok(mut b) = self.0.busy.lock() else {
                return;
            };
            b.gen += 1;
            b.active = true;
            b.gen
        };
        let me = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(me.0.after);
            let Ok(mut b) = me.0.busy.lock() else { return };
            if b.active && b.gen == gen && b.shown_at.is_none() {
                b.shown_at = Some(Instant::now());
                me.0.hint.set("⋯");
            }
        });
    }

    /// The request finished (answer, error or cancelled).
    pub fn end(&self) {
        let shown = {
            let Ok(mut b) = self.0.busy.lock() else {
                return;
            };
            b.active = false;
            b.shown_at
        };
        let Some(t) = shown else { return };
        let me = self.clone();
        let wait = self.0.min.saturating_sub(t.elapsed());
        std::thread::spawn(move || {
            std::thread::sleep(wait);
            let Ok(mut b) = me.0.busy.lock() else { return };
            if !b.active && b.shown_at == Some(t) {
                b.shown_at = None;
                me.0.hint.set(&me.normal());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> (Status, HintText) {
        let h = HintText::new("?");
        (
            Status::with_timing(
                h.clone(),
                Duration::from_millis(40),
                Duration::from_millis(80),
            ),
            h,
        )
    }

    #[test]
    fn fast_requests_never_show_the_busy_mark() {
        let (s, h) = st();
        s.begin();
        std::thread::sleep(Duration::from_millis(10));
        s.end();
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(h.get(), "?");
    }

    #[test]
    fn slow_request_shows_for_at_least_the_minimum() {
        let (s, h) = st();
        s.begin();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(h.get(), "⋯");
        s.end(); // shown for ~20 ms so far: stays until 80 ms
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(h.get(), "⋯");
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(h.get(), "?");
    }

    #[test]
    fn job_count_goes_into_the_normal_text() {
        let (s, h) = st();
        s.set_jobs(2);
        assert_eq!(h.get(), "[2] ?");
        s.begin();
        std::thread::sleep(Duration::from_millis(60));
        s.set_jobs(1);
        assert_eq!(h.get(), "⋯");
        s.end();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(h.get(), "[1] ?");
        s.set_jobs(0);
        assert_eq!(h.get(), "?");
    }
}
