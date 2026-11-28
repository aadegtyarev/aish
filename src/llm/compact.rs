//! Conversation memory management: the context window, the size estimate,
//! and compaction (a model-written summary replaces the old turns).

use std::time::Duration;

use aish_llm::{Error, Event, Message, Part, Request, Role};

use super::ask::{convo_chars, masked_history, short_reason};
use super::context::{trim_middle, CHARS_PER_TOKEN};
use super::render::{fit_line, Renderer};
use super::{Llm, Target};
use crate::editor::wait::Waiter;
use crate::module::Task;

/// The window assumed when neither the config nor the server says.
pub const DEFAULT_WINDOW: u64 = 8192;

/// No model this small is in use: below this the window is not looked up.
const MIN_WINDOW: u64 = 2048;

/// Starts the summary message at the head of the memory.
pub const SUMMARY_MARK: &str = "[summary of the earlier conversation]";

pub const DEFAULT_PROMPT: &str = "You compress a conversation between a user and an assistant in a bash terminal into notes \
that replace it. Keep: the user's goal and constraints; commands that were run with their key results and exit codes; \
errors and their causes; the current cwd, host and state; decisions made; open questions and next steps. \
Drop chit-chat and repeated output. Use terse bullet points. Never invent anything that is not in the conversation. \
If an earlier summary is included, fold it into the new one. Reply with the notes only.";

/// Tokens in `chars` characters (a rough estimate).
pub fn tokens_of(chars: usize) -> usize {
    chars.div_ceil(CHARS_PER_TOKEN)
}

/// Estimated size of the memory in tokens.
pub fn est_tokens(c: &[Message]) -> usize {
    tokens_of(convo_chars(c))
}

/// Does a conversation of `tokens` exceed the share `at` (0..1) of `window`?
pub fn needs_compaction(tokens: usize, window: u64, at: f64) -> bool {
    tokens as f64 > window as f64 * at
}

/// Token limit of the summary: `size` (0..1) of the window, at most 2048 and
/// a quarter of the summarising model's own window.
pub fn summary_tokens(window: u64, size: f64, summariser_window: u64) -> u32 {
    ((window as f64 * size) as u64)
        .min(2048)
        .min(summariser_window / 4)
        .max(64) as u32
}

/// Tokens a turn costs besides the memory: the context budget, the system
/// prompt and the tool schemas.
pub fn overhead_tokens(llm: &Llm, t: &Target, window: u64) -> usize {
    use super::agent;
    let mut chars = super::ask::SYSTEM.len();
    if t.tools {
        let run = llm.tool_on("run").then(|| llm.run_mode());
        let files = llm.tool_on("files");
        chars += agent::agent_guide(run, files).len();
        let web = llm.tool_on("web");
        if web {
            chars += agent::WEB_GUIDE.len();
        }
        let web_defs = if web {
            agent::web_defs(true, true)
        } else {
            Vec::new()
        };
        for d in agent::enabled_defs(run, files)
            .iter()
            .chain(web_defs.iter())
        {
            chars += d.name.len() + d.description.len() + d.parameters.to_string().len();
        }
    }
    super::context::budget_tokens(Some(window)) + tokens_of(chars)
}

/// The memory is a lone summary: there is nothing new to fold in.
pub fn nothing_new(c: &[Message]) -> bool {
    c.is_empty() || (c.len() == 1 && is_summary(&c[0]))
}

/// A summary made from unmasked text must not reach a non-local model:
/// mask it first (what the masker does not recognise is not a secret to it).
pub fn secure_memory(llm: &Llm, target_local: bool) {
    if target_local
        || !llm
            .summary_raw
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    if let Ok(mut c) = llm.convo.lock() {
        for m in c.iter_mut().filter(|m| is_summary(m)) {
            *m = Message::user(super::mask::mask(&m.text()));
        }
    }
}

/// `n_ctx` from a llama.cpp `/props` answer.
pub fn parse_props(body: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v["default_generation_settings"]["n_ctx"]
        .as_u64()
        .or_else(|| v["n_ctx"].as_u64())
        .filter(|n| *n > 0)
        .map(|n| n.clamp(2048, 4_000_000))
}

/// `GET <base>/props` for the provider URL `http://host:port/v1`.
pub fn props_url(url: &str) -> String {
    let u = url.trim_end_matches('/');
    let u = u.strip_suffix("/v1").unwrap_or(u);
    format!("{u}/props")
}

/// Ask a llama.cpp server for its context size; any failure is `None`.
pub fn fetch_n_ctx(url: &str, key: &Option<String>) -> Option<u64> {
    let mut r = ureq::get(&props_url(url)).timeout(Duration::from_millis(1500));
    if let Some(k) = key {
        r = r.set("Authorization", &format!("Bearer {k}"));
    }
    parse_props(&r.call().ok()?.into_string().ok()?)
}

/// `12.4k` for 12400, `850` for 850.
pub fn fmt_tokens(n: usize) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// `32k` for a window (exact multiples of 1024 read as such).
pub fn fmt_window(w: u64) -> String {
    if w >= 1000 && w.is_power_of_two() {
        format!("{}k", w / 1024)
    } else if w >= 1000 {
        format!("{}k", (w + 500) / 1000)
    } else {
        w.to_string()
    }
}

pub fn is_summary(m: &Message) -> bool {
    m.role == Role::User && m.text().starts_with(SUMMARY_MARK)
}

/// The conversation as plain text for the summary request.
pub fn transcript(c: &[Message]) -> String {
    let mut s = String::new();
    for m in c {
        let who = if m.role == Role::User {
            "user"
        } else {
            "assistant"
        };
        for p in &m.parts {
            match p {
                Part::Text(t) => s.push_str(&format!("[{who}] {t}\n")),
                Part::ToolCall(c) => {
                    s.push_str(&format!("[assistant ran {}] {}\n", c.name, c.arguments))
                }
                Part::ToolResult { content, .. } => s.push_str(&format!("[result] {content}\n")),
            }
        }
    }
    s
}

/// Consecutive plain user messages become one (a summary followed by the
/// next question: some chat templates insist on alternating roles).
pub fn merge_user_runs(v: Vec<Message>) -> Vec<Message> {
    let plain =
        |m: &Message| m.role == Role::User && m.parts.iter().all(|p| matches!(p, Part::Text(_)));
    let mut out: Vec<Message> = Vec::with_capacity(v.len());
    for m in v {
        match out.last_mut() {
            Some(last) if plain(last) && plain(&m) => {
                let joined = format!("{}\n\n{}", last.text(), m.text());
                *last = Message::user(joined);
            }
            _ => out.push(m),
        }
    }
    out
}

/// Drop whole old turns until the memory is within `max_tokens` (the last
/// turn always stays). True if anything was dropped.
pub fn drop_oldest(c: &mut Vec<Message>, max_tokens: usize) -> bool {
    let mut dropped = false;
    while c.len() > 1 && est_tokens(c) > max_tokens {
        let next = c.iter().skip(1).position(|m| {
            m.role == Role::User && matches!(m.parts.first(), Some(Part::Text(_))) && !is_summary(m)
        });
        match next {
            Some(i) => {
                c.drain(..=i);
                dropped = true;
            }
            None => break,
        }
    }
    dropped
}

fn prompt_of(llm: &Llm) -> String {
    llm.config()
        .and_then(|c| c.compact.as_ref()?.prompt.clone())
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PROMPT.to_owned())
}

/// Summarise the memory and replace it with the summary; the caller holds
/// the turn lock. Returns the estimated size before and after, in tokens.
pub fn compact(llm: &Llm, chat: &Target, task: &Task) -> Result<(usize, usize), Error> {
    use std::sync::atomic::Ordering::SeqCst;
    let convo = llm
        .convo
        .lock()
        .map_err(|_| Error::Api("memory".into()))?
        .clone();
    let before = est_tokens(&convo);
    let ct = llm.compact_target(chat);
    let size = llm.config().map_or(0.1, |c| c.compact_size());
    let ct_window = llm.window(&ct);
    let budget = summary_tokens(llm.window(chat), size, ct_window);
    // The summary may later go to any model of the chat role: unless all of
    // them (and the summariser) are local, the text is masked.
    let all_local = ct.local && chat.local && llm.targets("chat").iter().all(|t| t.local);
    let text = transcript(&masked_history(&convo, all_local));
    let room = (ct_window as usize)
        .saturating_sub(budget as usize + 600)
        .saturating_mul(CHARS_PER_TOKEN);
    let user = format!(
        "Conversation:\n{}\n\nWrite the notes now, at most {budget} tokens.",
        trim_middle(&text, room.max(2000))
    );
    let req = Request {
        max_tokens: budget,
        ..Request::chat(prompt_of(llm), vec![Message::user(user)])
    };
    let mut w = Waiter::start_compacting(task);
    let mut out = String::new();
    let r = aish_llm::stream(&ct.ep, &req, &task.shared.cancel, |e| {
        w.event(&e);
        if let Event::Delta(d) = e {
            out.push_str(&d);
        }
    });
    w.stop();
    r?;
    let summary = out.trim();
    if summary.is_empty() {
        return Err(Error::Api("empty summary".into()));
    }
    let new = vec![Message::user(format!("{SUMMARY_MARK}\n{summary}"))];
    let after = est_tokens(&new);
    let mut c = llm.convo.lock().map_err(|_| Error::Api("memory".into()))?;
    *c = new;
    llm.summary_raw.store(all_local, SeqCst);
    Ok((before, after))
}

fn mark(task: &Task, text: &str) {
    let r = Renderer::new(&task.theme, task.cols);
    let mut b = Vec::new();
    r.mark(
        &fit_line(text, usize::from(task.cols).saturating_sub(3)),
        &mut b,
    );
    task.write(&b);
}

fn done_line(before: usize, after: usize) -> String {
    format!(
        "compacted {} → {} tokens",
        fmt_tokens(before),
        fmt_tokens(after)
    )
}

/// `?c`: compact now. The caller holds the turn lock.
pub fn run_command(llm: &Llm, chat: &Target, task: &Task) {
    let empty = llm.convo.lock().is_ok_and(|c| nothing_new(&c));
    if empty {
        return mark(task, "nothing to compact");
    }
    match compact(llm, chat, task) {
        Ok((a, b)) => mark(task, &done_line(a, b)),
        Err(Error::Cancelled) => mark(task, "⏹"),
        Err(e) => mark(
            task,
            &format!("compact failed · {}", short_reason(&e, chat)),
        ),
    }
}

/// Before a turn: compact when the memory is over `compact.at` of the
/// window; if that fails, drop the oldest turns. False = cancelled (stop).
/// The caller holds the turn lock.
pub fn before_turn(llm: &Llm, chat: &Target, task: &Task) -> bool {
    secure_memory(llm, chat.local);
    let (tokens, nothing) = llm
        .convo
        .lock()
        .map_or((0, true), |c| (est_tokens(&c), nothing_new(&c)));
    if nothing {
        return true;
    }
    let at = llm.config().map_or(0.7, |c| c.compact_at());
    // (a small memory fits any real window: the server is not asked for it yet)
    let over = |w: u64| needs_compaction(tokens + overhead_tokens(llm, chat, w), w, at);
    if chat.window.is_none() && !over(MIN_WINDOW) {
        return true;
    }
    let window = llm.window(chat);
    if !over(window) {
        return true;
    }
    match compact(llm, chat, task) {
        Ok((a, b)) => mark(task, &done_line(a, b)),
        Err(Error::Cancelled) => {
            mark(task, "⏹");
            return false;
        }
        Err(e) => {
            tracing::warn!("compaction: {e:?}");
            let keep = ((window as f64 * at / 2.0) as usize)
                .saturating_sub(overhead_tokens(llm, chat, window));
            if let Ok(mut c) = llm.convo.lock() {
                drop_oldest(&mut c, keep);
            }
            mark(task, "not compacted · dropped old turns");
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_math() {
        assert!(!needs_compaction(5734, 8192, 0.7)); // 5734.4
        assert!(needs_compaction(5735, 8192, 0.7));
        assert!(!needs_compaction(0, 8192, 0.7));
        assert!(needs_compaction(300, 400, 0.7));
        assert_eq!(tokens_of(0), 0);
        assert_eq!(tokens_of(1), 1);
        assert_eq!(tokens_of(400), 100);
        // min(size of the window, 2048, a quarter of the summariser's window)
        assert_eq!(summary_tokens(32768, 0.1, 32768), 2048);
        assert_eq!(summary_tokens(8192, 0.1, 8192), 819);
        assert_eq!(summary_tokens(32768, 0.1, 4096), 1024);
        assert_eq!(summary_tokens(1000, 0.1, 8192), 100);
    }

    #[test]
    fn props_parsing() {
        let p = r#"{"default_generation_settings":{"n_ctx":32768,"x":1},"total_slots":1}"#;
        assert_eq!(parse_props(p), Some(32768));
        assert_eq!(parse_props(r#"{"n_ctx":4096}"#), Some(4096));
        assert_eq!(parse_props(r#"{"n_ctx":0}"#), None);
        assert_eq!(parse_props(r#"{"n_ctx":512}"#), Some(2048));
        assert_eq!(parse_props(r#"{"n_ctx":99999999}"#), Some(4_000_000));
        assert_eq!(parse_props("not json"), None);
        assert_eq!(parse_props("{}"), None);
        assert_eq!(props_url("http://h:8080/v1"), "http://h:8080/props");
        assert_eq!(props_url("http://h:8080/v1/"), "http://h:8080/props");
        assert_eq!(props_url("http://h:8080"), "http://h:8080/props");
    }

    #[test]
    fn number_formats() {
        assert_eq!(fmt_tokens(850), "850");
        assert_eq!(fmt_tokens(12400), "12.4k");
        assert_eq!(fmt_window(32768), "32k");
        assert_eq!(fmt_window(8192), "8k");
        assert_eq!(fmt_window(128000), "128k");
        assert_eq!(fmt_window(512), "512");
    }

    #[test]
    fn summary_and_merging() {
        let s = Message::user(format!("{SUMMARY_MARK}\n- goal"));
        assert!(is_summary(&s) && !is_summary(&Message::user("hi")));
        let m = merge_user_runs(vec![s, Message::user("q")]);
        assert_eq!(m.len(), 1);
        assert!(m[0].text().ends_with("\n\nq") && m[0].text().contains("goal"));
        let m = merge_user_runs(vec![
            Message::user("a"),
            Message::assistant("b"),
            Message::user("c"),
        ]);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn dropping_keeps_whole_turns_and_the_last_one() {
        let mut c = Vec::new();
        for i in 0..4 {
            c.push(Message::user(format!("q{i} {}", "x".repeat(400))));
            c.push(Message::assistant(format!("a{i} {}", "y".repeat(400))));
        }
        assert!(drop_oldest(&mut c, 250));
        assert!(c.len() == 2 && c[0].text().starts_with("q3"), "{}", c.len());
        assert!(!drop_oldest(&mut c, 1)); // never the last turn
    }

    #[test]
    fn reset_waits_for_a_running_turn() {
        let llm = Llm::new();
        *llm.convo.lock().unwrap() = vec![Message::user("q")];
        {
            let _busy = llm.turn.lock().unwrap(); // a turn or a compaction
            assert!(!llm.reset_memory());
            assert_eq!(llm.convo.lock().unwrap().len(), 1);
        }
        assert!(llm.reset_memory());
        assert!(llm.convo.lock().unwrap().is_empty());
    }

    #[test]
    fn a_lone_summary_has_nothing_new() {
        let s = Message::user(format!("{SUMMARY_MARK}\nx"));
        assert!(nothing_new(&[]) && nothing_new(std::slice::from_ref(&s)));
        assert!(!nothing_new(&[s, Message::user("q")]));
        assert!(!nothing_new(&[Message::user("q")]));
    }

    #[test]
    fn an_unmasked_summary_is_masked_before_a_remote_model() {
        use std::sync::atomic::Ordering::SeqCst;
        let llm = Llm::new();
        let text = format!("{SUMMARY_MARK}\n- ran with API_KEY=abc123secret");
        *llm.convo.lock().unwrap() = vec![Message::user(text)];
        llm.summary_raw.store(true, SeqCst);
        secure_memory(&llm, true); // a local model may read it as it is
        assert!(llm.convo.lock().unwrap()[0].text().contains("abc123secret"));
        secure_memory(&llm, false);
        let c = llm.convo.lock().unwrap();
        assert!(
            !c[0].text().contains("abc123secret") && is_summary(&c[0]),
            "{}",
            c[0].text()
        );
        assert!(!llm.summary_raw.load(SeqCst));
    }

    #[test]
    fn overhead_counts_the_budget_system_prompt_and_tools() {
        let ep = aish_llm::Endpoint {
            kind: aish_llm::Kind::Chat,
            url: "http://x/v1".into(),
            key: None,
            model: "m".into(),
            effort: None,
        };
        let llm = Llm::new();
        let mut t = Target {
            ep,
            provider: "p".into(),
            label: "p/m".into(),
            window: None,
            local: true,
            tools: false,
            native_search: false,
        };
        let plain = overhead_tokens(&llm, &t, 8192);
        t.tools = true;
        assert!(overhead_tokens(&llm, &t, 8192) > plain + 300);
        assert!(plain > super::super::context::budget_tokens(Some(8192)));
    }

    #[test]
    fn transcript_names_the_speakers() {
        let t = transcript(&[Message::user("hi"), Message::assistant("yo")]);
        assert_eq!(t, "[user] hi\n[assistant] yo\n");
    }
}
