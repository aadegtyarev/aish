//! The waiting indicator of an answer: one dim status row where the answer
//! will start, `│ ⠹ thinking · 4s`, updated in place and replaced by the first
//! output row (see `Task::write`).

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aish_llm::{Endpoint, Event};

use crate::llm::render::fit_line;
use crate::module::{StatusWriter, Task};

const TICK: Duration = Duration::from_millis(125);
const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// The model server is asked about its state after this long without a byte.
const PROBE_AFTER: Duration = Duration::from_secs(1);
const PROBE_EVERY: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Connecting = 0,
    Waiting = 1,
    Loading = 2,
    Thinking = 3,
    /// Summarising the conversation (`◇ ⠹ compacting…`).
    Compacting = 4,
}

impl Phase {
    fn from(n: u8) -> Phase {
        match n {
            1 => Phase::Waiting,
            2 => Phase::Loading,
            3 => Phase::Thinking,
            4 => Phase::Compacting,
            _ => Phase::Connecting,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Phase::Connecting => "connecting",
            Phase::Waiting => "waiting",
            Phase::Loading => "loading model",
            Phase::Thinking => "thinking",
            Phase::Compacting => "compacting…",
        }
    }
}

/// The status text for one tick, cut to `cols` (the bar takes two): the
/// elapsed time goes first, then the Ctrl-C hint, never a wrap.
pub fn status_text(phase: Phase, secs: u64, tick: usize, color: bool, cols: usize) -> String {
    let spin = if color {
        FRAMES[tick % FRAMES.len()].to_owned()
    } else {
        ".".repeat(tick % 3 + 1)
    };
    let room = cols.saturating_sub(2 + 1);
    let base = format!("{spin} {}", phase.label());
    let mut parts = vec![base];
    if secs >= 1 {
        parts.push(format!("{secs}s"));
    }
    if secs >= 10 {
        parts.push("Ctrl-C — cancel".to_owned());
    }
    // drop the hint, then the time, while it does not fit
    while parts.len() > 1 && parts.join(" · ").chars().count() > room {
        parts.pop();
    }
    fit_line(&parts.join(" · "), room + 1)
}

/// A running wait: a ticker thread that redraws the status row.
pub struct Waiter {
    phase: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Waiter {
    /// `probe`: the local model server to ask whether the model is loading.
    pub fn start(task: &Task, probe: Option<Endpoint>) -> Waiter {
        Self::start_with(task, probe, Phase::Connecting, bar_of(task))
    }

    /// The wait of a conversation summary: `◇ ⠹ compacting… · 4s`.
    pub fn start_compacting(task: &Task) -> Waiter {
        let bar = if task.theme.enabled {
            format!("\x1b[{}m◇ \x1b[0m", task.theme.dim)
        } else {
            "◇ ".to_owned()
        };
        Self::start_with(task, None, Phase::Compacting, bar)
    }

    fn start_with(task: &Task, probe: Option<Endpoint>, first: Phase, bar: String) -> Waiter {
        let w = task.status_writer();
        w.reset();
        let phase = Arc::new(AtomicU8::new(first as u8));
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s) = (phase.clone(), stop.clone());
        let (color, cols) = (task.theme.enabled, usize::from(task.cols));
        let thread = std::thread::spawn(move || ticker(w, p, s, probe, color, bar, cols));
        Waiter {
            phase,
            stop,
            thread: Some(thread),
        }
    }

    /// Feed the stream's events; progress events move the phase.
    pub fn event(&self, e: &Event) {
        let to = match e {
            Event::Connected => Phase::Waiting,
            Event::Thinking => Phase::Thinking,
            _ => return,
        };
        let cur = Phase::from(self.phase.load(Relaxed));
        if cur == Phase::Compacting {
            return;
        }
        // loading is a more specific "waiting"; thinking stays once seen
        if to == Phase::Waiting && cur != Phase::Connecting {
            return;
        }
        self.phase.store(to as u8, Relaxed);
    }

    /// The stream is over (any outcome). The row stays until output replaces it.
    pub fn stop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        self.stop();
    }
}

fn bar_of(task: &Task) -> String {
    if task.theme.enabled {
        format!("\x1b[{}m│ \x1b[0m", task.theme.dim)
    } else {
        "│ ".to_owned()
    }
}

fn ticker(
    w: StatusWriter,
    phase: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    probe: Option<Endpoint>,
    color: bool,
    bar: String,
    cols: usize,
) {
    let start = Instant::now();
    let mut last = String::new();
    let mut tick = 0usize;
    let mut next_probe = PROBE_AFTER;
    while !stop.load(Relaxed) {
        let p = Phase::from(phase.load(Relaxed));
        let el = start.elapsed();
        if let Some(ep) = &probe {
            if el >= next_probe && matches!(p, Phase::Waiting | Phase::Loading) {
                next_probe = el + PROBE_EVERY;
                match model_state(ep) {
                    Some(true) => phase.store(Phase::Loading as u8, Relaxed),
                    Some(false) if p == Phase::Loading => {
                        phase.store(Phase::Waiting as u8, Relaxed)
                    }
                    _ => {}
                }
                if stop.load(Relaxed) {
                    break;
                }
            }
        }
        let p = Phase::from(phase.load(Relaxed));
        let text = status_text(p, el.as_secs(), tick, color, cols);
        // the spinner turns with the text; the row is redrawn only when it changed
        if text != last {
            let dim = if color {
                ("\x1b[2m", "\x1b[0m")
            } else {
                ("", "")
            };
            let row = format!("{bar}{}{text}{}", dim.0, dim.1);
            if !w.show(row.as_bytes()) {
                return;
            }
            last = text;
        }
        tick += 1;
        std::thread::sleep(TICK);
    }
}

/// llama.cpp router mode: is the target model `unloaded`/`loading`? None when
/// the server does not say (the field is absent, or it cannot be asked).
fn model_state(ep: &Endpoint) -> Option<bool> {
    let url = format!("{}/models", ep.url.trim_end_matches('/'));
    let mut r = ureq::get(&url).timeout(Duration::from_secs(2));
    if let Some(k) = &ep.key {
        r = r.set("Authorization", &format!("Bearer {k}"));
    }
    let v: serde_json::Value = serde_json::from_str(&r.call().ok()?.into_string().ok()?).ok()?;
    let m = v["data"]
        .as_array()?
        .iter()
        .find(|m| m["id"].as_str() == Some(ep.model.as_str()))?;
    let s = m["status"]["value"].as_str()?;
    Some(matches!(s, "unloaded" | "loading"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_shrinks_before_it_wraps() {
        let full = status_text(Phase::Thinking, 12, 2, true, 80);
        assert_eq!(full, "⠹ thinking · 12s · Ctrl-C — cancel");
        assert_eq!(status_text(Phase::Thinking, 0, 2, true, 80), "⠹ thinking");
        // 36 columns: the hint does not fit, the time does
        let t = status_text(Phase::Loading, 12, 0, true, 36);
        assert_eq!(t, "⠋ loading model · 12s");
        // very narrow: only the phase, cut with an ellipsis if it must be
        let t = status_text(Phase::Loading, 12, 0, true, 14);
        assert!(t.chars().count() < 14 - 2, "{t}");
        assert_eq!(status_text(Phase::Waiting, 0, 2, false, 80), "... waiting");
    }
}
