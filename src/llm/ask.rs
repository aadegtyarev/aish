//! `?` questions: context, request, streaming render, command handoff.

use std::sync::{Arc, Mutex};

use aish_llm::{Error, Message, Part};

use super::agent;
use super::context;
use super::mask::mask;
use super::render::Renderer;
use super::{Llm, Target};
use crate::blocks::BlockTracker;
use crate::module::Task;

pub(crate) const SYSTEM: &str = "You are a terse, practical assistant inside the user's bash terminal. \
Answer in the language of the user's question. Use light markdown only (**bold**, `code`, short lists). \
When a shell command is the answer, put exactly one command in a final ```bash fenced block at the end of the reply \
(multi-line only if unavoidable). If the user only asks for a command, reply with just that block and no prose. \
You cannot run anything: the user reads the command and presses Enter. \
Use the provided context (recent commands and their output, cwd, OS, git) instead of asking for it.";

/// Conversation size cap (characters, ~4 per token).
const CONVO_CAP: usize = 24_000;

pub struct Parsed {
    pub reset: bool,
    pub blocks: usize,
    pub question: String,
}

/// Split a `?` line into reset flag, block count and question text.
pub fn parse(line: &str) -> Parsed {
    let r = line.strip_prefix('?').unwrap_or(line);
    let (reset, r) = match r.strip_prefix('!') {
        Some(x) => (true, x),
        None => (false, r),
    };
    let digits = r.chars().take_while(char::is_ascii_digit).count();
    let blocks = r[..digits].parse().unwrap_or(1usize).clamp(1, 20);
    Parsed {
        reset,
        blocks: if digits == 0 { 1 } else { blocks },
        question: r[digits..].trim().to_owned(),
    }
}

/// Short, user-facing reason for an error.
pub fn short_reason(e: &Error, t: &Target) -> String {
    match e {
        Error::Connect(_) | Error::Timeout => format!("no answer from {}", t.label),
        Error::Http { status, message } if matches!(status, 401 | 403) => {
            let _ = message;
            format!("{status} from {}", t.provider)
        }
        Error::Http { status, message } => {
            let m: String = message.chars().take(60).collect();
            format!("{status} from {}: {m}", t.provider)
        }
        Error::Api(m) => format!("{}: {}", t.label, m.chars().take(60).collect::<String>()),
        Error::Cancelled => "⏹".into(),
    }
}

/// Earlier turns as sent: masked unless the provider is local. The stored
/// conversation stays untouched.
pub fn masked_history(c: &[Message], local: bool) -> Vec<Message> {
    if local {
        return c.to_vec();
    }
    c.iter()
        .map(|m| Message {
            role: m.role,
            parts: m
                .parts
                .iter()
                .map(|p| match p {
                    Part::Text(t) => Part::Text(mask(t)),
                    Part::ToolCall(c) => Part::ToolCall(aish_llm::ToolCall {
                        arguments: mask(&c.arguments),
                        ..c.clone()
                    }),
                    Part::ToolResult { id, content } => Part::ToolResult {
                        id: id.clone(),
                        content: mask(content),
                    },
                })
                .collect(),
        })
        .collect()
}

fn msg_chars(m: &Message) -> usize {
    m.parts
        .iter()
        .map(|p| match p {
            Part::Text(t) => t.len(),
            Part::ToolCall(c) => c.arguments.len() + c.name.len(),
            Part::ToolResult { content, .. } => content.len(),
        })
        .sum()
}

fn convo_chars(c: &[Message]) -> usize {
    c.iter().map(msg_chars).sum()
}

/// Run `f` on a worker thread; a panic must not leave the terminal waiting:
/// the task's drop guard (or this) hands the prompt back with `f`'s prefill.
pub fn run_task(task: Task, f: impl FnOnce(&Task) -> Option<String> + Send + 'static) {
    std::thread::spawn(move || {
        // a panic here is caught below: the panic hook must not touch the terminal
        crate::term::IN_GUARD.with(|x| x.set(true));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&task)));
        match r {
            Ok(prefill) => task.finish(prefill),
            Err(e) => {
                let msg = e
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| e.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                tracing::error!("ask worker panicked: {msg}");
                task.finish(None);
            }
        }
    });
}

/// Start answering on a worker thread.
pub fn spawn(
    llm: Arc<Llm>,
    tracker: Arc<Mutex<BlockTracker>>,
    target: Target,
    p: Parsed,
    task: Task,
) {
    run_task(task, move |task| answer(&llm, &tracker, &target, &p, task));
}

fn answer(
    llm: &Llm,
    tracker: &Arc<Mutex<BlockTracker>>,
    t: &Target,
    p: &Parsed,
    task: &Task,
) -> Option<String> {
    let mut ctx = context::build(tracker, p.blocks, t.window);
    let jobs = llm.jobs.lock().map(|j| j.describe()).unwrap_or_default();
    if !jobs.starts_with("no ") {
        ctx.push_str(&format!("background jobs:\n{jobs}\n"));
    }
    // (masking happens per request, for the model that actually answers)
    let q = if p.question.is_empty() {
        "Explain what went wrong or what the last output means, briefly.".to_owned()
    } else {
        p.question.clone()
    };
    let _turn = llm.turn.lock().unwrap_or_else(|e| e.into_inner());
    let history = llm.convo.lock().map(|c| c.clone()).unwrap_or_default();
    let turn = agent::start_turn(history, format!("Context:\n{ctx}\nQuestion: {q}"), q);
    agent::drive(llm, tracker, t, turn, task)
}

/// Lines of the running program's output the overlay sends at most.
pub const OVERLAY_LINES: usize = 200;

/// What the Ctrl-Q overlay knows about the running program.
#[derive(Debug, Clone, Default)]
pub struct Screen {
    /// The command line that was started (unknown if bash ran it natively).
    pub cmd: Option<String>,
    pub cwd: String,
    /// The program is in the alternate screen: the output stream does not
    /// tell what is on the screen.
    pub alt: bool,
    /// Tail of the output stream (at most `OVERLAY_LINES` lines).
    pub tail: String,
}

/// The last `lines` lines of `s` within `max_chars` (cut at a line start where
/// possible), and how many lines that is.
pub fn tail_lines(s: &str, lines: usize, max_chars: usize) -> (String, usize) {
    let all: Vec<&str> = s.trim_end().lines().collect();
    let mut from = all.len().saturating_sub(lines);
    let mut total: usize = all[from..].iter().map(|l| l.chars().count() + 1).sum();
    while total > max_chars && from + 1 < all.len() {
        total -= all[from].chars().count() + 1;
        from += 1;
    }
    let mut t = all[from..].join("\n");
    let n = t.chars().count();
    if n > max_chars {
        t = t.chars().skip(n - max_chars).collect();
    }
    (t, all.len() - from)
}

/// The context text of an overlay question (unmasked).
pub fn overlay_context(s: &Screen, window: Option<u64>) -> String {
    let max = context::budget_tokens(window) * 4;
    let (tail, _) = tail_lines(&s.tail, OVERLAY_LINES, max);
    let mut c = format!(
        "A program is running in the terminal.\nrunning command: {}\nlocal shell cwd: {}\n",
        s.cmd.as_deref().unwrap_or("(unknown)"),
        s.cwd
    );
    if s.alt {
        c.push_str(
            "The program is in the alternate screen (full-screen/TUI): the text below is the raw output stream \
             and may not match what is on the screen now.\n",
        );
    }
    if tail.is_empty() {
        c.push_str("output: (none yet)\n");
    } else {
        c.push_str(&format!("last output:\n{tail}\n"));
    }
    c
}

/// Answer a question from the overlay (worker thread). Returns the command
/// of a final bash block, already shown under the answer.
pub fn overlay_answer(
    llm: &Llm,
    tracker: &Arc<Mutex<BlockTracker>>,
    t: &Target,
    screen: &Screen,
    question: &str,
    task: &Task,
) -> Option<String> {
    let ctx = overlay_context(screen, t.window);
    let q = if question.is_empty() {
        "Explain what is happening in the output above and what to do next, briefly.".to_owned()
    } else {
        question.to_owned()
    };
    let _turn = llm.turn.lock().unwrap_or_else(|e| e.into_inner());
    let history = llm.convo.lock().map(|c| c.clone()).unwrap_or_default();
    let mut turn = agent::start_turn(history, format!("Context:\n{ctx}\nQuestion: {q}"), q);
    turn.overlay = true;
    let cmd = agent::drive(llm, tracker, t, turn, task)?;
    let mut out = Vec::new();
    Renderer::new(&task.theme, task.cols).code_block(&cmd, &mut out);
    task.write(&out);
    Some(cmd)
}

/// Keep a finished turn: the bare question, the tool traffic, the answer.
pub fn remember_turn(llm: &Llm, turn: &agent::Turn) {
    let Ok(mut c) = llm.convo.lock() else { return };
    c.push(Message::user(turn.q.clone()));
    c.extend(turn.msgs.iter().skip(1).cloned());
    // drop whole old turns (up to the next plain question), never half of one
    while c.len() > 2 && convo_chars(&c) > CONVO_CAP {
        let next = c.iter().skip(1).position(|m| {
            m.role == aish_llm::Role::User && matches!(m.parts.first(), Some(Part::Text(_)))
        });
        match next {
            Some(i) => {
                c.drain(..=i);
            }
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_masked_for_remote_only() {
        let h = vec![
            Message::user("export API_KEY=abc123 why"),
            Message::assistant("use https://u:pw@h/x"),
        ];
        let m = masked_history(&h, false);
        assert_eq!(m[0].text(), "export API_KEY=*** why");
        assert_eq!(m[1].text(), "use https://u:***@h/x");
        assert_eq!(masked_history(&h, true), h);
        assert_eq!(h[0].text(), "export API_KEY=abc123 why"); // stored copy intact
    }

    #[test]
    fn tail_keeps_the_end_within_limits() {
        let s: String = (1..=300).map(|i| format!("line{i}\n")).collect();
        let (t, n) = tail_lines(&s, 200, 100_000);
        assert_eq!(n, 200);
        assert!(t.starts_with("line101\n") && t.ends_with("line300"));
        let (t, n) = tail_lines(&s, 200, 40);
        assert!(
            t.chars().count() <= 40 && n < 10 && t.ends_with("line300"),
            "{t:?}"
        );
        assert_eq!(tail_lines("", 200, 100), (String::new(), 0));
    }

    #[test]
    fn overlay_context_says_when_the_screen_is_unreliable() {
        let mut s = Screen {
            cmd: Some("vim x".into()),
            cwd: "/h".into(),
            alt: true,
            tail: "text".into(),
        };
        let c = overlay_context(&s, None);
        assert!(c.contains("vim x") && c.contains("alternate screen") && c.contains("text"));
        s.alt = false;
        s.tail.clear();
        let c = overlay_context(&s, None);
        assert!(!c.contains("alternate") && c.contains("none yet"));
    }

    #[test]
    fn parsing() {
        let p = parse("?");
        assert!(!p.reset && p.blocks == 1 && p.question.is_empty());
        let p = parse("?3 why  is it slow");
        assert!(p.blocks == 3 && p.question == "why  is it slow");
        let p = parse("?! new");
        assert!(p.reset && p.question == "new");
        let p = parse("? hello");
        assert_eq!((p.blocks, p.question.as_str()), (1, "hello"));
    }
}
