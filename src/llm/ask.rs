//! `?` questions: context, request, streaming render, command handoff.

use std::sync::{Arc, Mutex};

use aish_llm::{Error, Message, Part};

use super::agent;
use super::context;
use super::mask::mask;
use super::{Llm, Target};
use crate::blocks::BlockTracker;
use crate::module::Task;

pub(crate) const SYSTEM: &str = "You are a terse, practical assistant inside the user's bash terminal. \
Answer in the user's language. Use light markdown only (**bold**, `code`, short lists). \
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
        Error::Connect(_) | Error::Timeout => format!("нет ответа от {}", t.label),
        Error::Http { status, message } if matches!(status, 401 | 403) => {
            let _ = message;
            format!("{status} от {}", t.provider)
        }
        Error::Http { status, message } => {
            let m: String = message.chars().take(60).collect();
            format!("{status} от {}: {m}", t.provider)
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
    if !t.local {
        ctx = mask(&ctx);
    }
    let q = if p.question.is_empty() {
        "Explain what went wrong or what the last output means, briefly.".to_owned()
    } else if t.local {
        p.question.clone()
    } else {
        mask(&p.question)
    };
    let history = llm.convo.lock().map(|c| c.clone()).unwrap_or_default();
    let turn = agent::start_turn(history, format!("Context:\n{ctx}\nQuestion: {q}"), q);
    agent::drive(llm, tracker, t, turn, task)
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
