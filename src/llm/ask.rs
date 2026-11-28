//! Questions to the model: context, request, streaming render, command handoff.

use std::sync::{Arc, Mutex};

use aish_llm::{Error, Message, Part};

use super::agent;
use super::context;
use super::mask::mask;
use super::render::Renderer;
use super::{Llm, Target};
use crate::blocks::BlockTracker;
use crate::task::Task;

pub(crate) const SYSTEM: &str =
    "You are a terse assistant inside aish, an AI wrapper around the user's bash terminal. \
Use light markdown (**bold**, `code`, short lists). \
Use the given context (recent commands and output, cwd, OS, git) instead of asking for it; \
answer the question asked, not the last error. \
Commands must run as is, no <placeholders>; ask if a value is unknown. \
Never claim or undo a change no command made; say so if nothing changed. \
About aish itself read /usr/share/doc/aish/ or github.com/aadegtyarev/aish instead of guessing; \
`aish help` lists its commands.";

/// A question: how many blocks of context, and the text.
pub struct Parsed {
    pub blocks: usize,
    pub question: String,
}

/// Short, user-facing reason for an error.
pub fn short_reason(e: &Error, t: &Target) -> String {
    match e {
        Error::Connect(_) | Error::Timeout => format!("no answer from {}", t.label),
        Error::Http {
            status: status @ (401 | 403),
            ..
        } => format!("{status} from {}", t.provider),
        Error::Http { status, message } => {
            format!(
                "{status} from {}: {}",
                t.provider,
                message.chars().take(60).collect::<String>()
            )
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

pub(crate) fn convo_chars(c: &[Message]) -> usize {
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
    run_task(task, move |task| {
        let q = match p.question.as_str() {
            "" => "Explain what went wrong or what the last output means, briefly.",
            q => q,
        };
        let mut ctx = context::build(&tracker, p.blocks, target.window);
        let jobs = llm.jobs.lock().map(|j| j.describe()).unwrap_or_default();
        if !jobs.starts_with("no ") {
            ctx.push_str(&format!("background jobs:\n{jobs}\n"));
        }
        run_turn(&llm, &tracker, &target, task, ctx, q.to_owned(), false)
    });
}

/// One question: compact if needed, start the turn with `ctx`, drive it.
fn run_turn(
    llm: &Llm,
    tracker: &Arc<Mutex<BlockTracker>>,
    t: &Target,
    task: &Task,
    ctx: String,
    q: String,
    overlay: bool,
) -> Option<String> {
    let _turn = llm.turn.lock().unwrap_or_else(|e| e.into_inner());
    if !super::compact::before_turn(llm, t, task) {
        return None;
    }
    let history = llm.convo.lock().map(|c| c.clone()).unwrap_or_default();
    // a narrow phone screen wraps wide tables into noise
    let screen = if task.cols < 80 {
        format!(
            "screen: {} columns: keep lines short, no wide tables\n",
            task.cols
        )
    } else {
        String::new()
    };
    // right next to the question small models follow it far more often
    let nudge = if t.tools && !overlay {
        "\n(Get the facts with your tools, then answer.)"
    } else {
        ""
    };
    let msg = format!("Context:\n{screen}{ctx}\nQuestion: {q}{nudge}");
    let mut turn = agent::start_turn(history, msg, q);
    turn.overlay = overlay;
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
    let max = context::budget_tokens(window) * context::CHARS_PER_TOKEN;
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
    let q = match question {
        "" => "Explain what is happening in the output above and what to do next, briefly.",
        q => q,
    };
    let ctx = overlay_context(screen, t.window);
    let cmd = run_turn(llm, tracker, t, task, ctx, q.to_owned(), true)?;
    let mut out = Vec::new();
    Renderer::new(&task.theme, task.cols).code_block(&cmd, &mut out);
    task.write(&out);
    Some(cmd)
}

/// Keep a finished turn: the bare question, the tool traffic, the answer.
/// (The memory is cut to `cap_tokens` as a last resort; compaction normally
/// acts long before.)
pub fn remember_turn(llm: &Llm, turn: &agent::Turn, cap_tokens: usize) {
    let Ok(mut c) = llm.convo.lock() else { return };
    c.push(Message::user(turn.q.clone()));
    c.extend(turn.msgs.iter().skip(1).cloned());
    super::compact::drop_oldest(&mut c, cap_tokens);
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
    fn memory_is_cut_at_the_cap_by_whole_turns() {
        let llm = Llm::new();
        for i in 0..5 {
            let mut turn = agent::start_turn(Vec::new(), "ctx".into(), format!("q{i}"));
            turn.msgs.push(Message::assistant("a".repeat(100)));
            remember_turn(&llm, &turn, 112);
        }
        let c = llm.convo.lock().unwrap();
        assert!(
            convo_chars(&c) <= 450 && c.len().is_multiple_of(2) && c[0].text().starts_with('q')
        );
    }
}
