//! The agent loop behind `?`: the model may call tools. Read-only commands
//! and file reads run on the spot; anything else is handed to the user's line
//! editor and the turn pauses until that command's block finishes.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use llmsh_llm::{Error, Event, Message, Request, Tool, ToolCall};
use serde_json::{json, Value};

use super::allow::{is_read_only, is_secret_path};
use super::ask::{masked_history, remember_turn, short_reason, SYSTEM};
use super::context::trim_middle;
use super::render::{unsafe_cmd, Renderer};
use super::{Llm, Target};
use crate::blocks::{Block, BlockTracker};
use crate::module::Task;

pub const MAX_STEPS: usize = 12;
const RUN_TIMEOUT: Duration = Duration::from_secs(10);
const OUT_CAP: usize = 16 * 1024;
const FILE_LINES: usize = 200;
const FILE_BYTES: usize = 32 * 1024;
const PENDING_TTL: Duration = Duration::from_secs(30 * 60);

pub const AGENT_GUIDE: &str = "\n\nYou can use tools. Prefer the read-only tools (run with a read-only command, \
read_file, read_block, jobs, job_output) to gather facts instead of guessing or asking the user. \
Propose exactly one mutating command at a time with run: it goes into the user's input line, \
they decide whether to run it, and you get the result afterwards. For long-running commands use \
run with background=true and check on them with jobs/job_output. Explain briefly what you are doing and why. \
Stop (answer without a tool call) as soon as the task is done. Do not put the command in a ```bash block when you use run.";

pub fn tool_defs() -> Vec<Tool> {
    let t = |name: &str, description: &str, parameters: Value| Tool {
        name: name.into(),
        description: description.into(),
        parameters,
    };
    vec![
        t(
            "run",
            "Run a shell command. Read-only commands (ls, cat, grep, git status, ...) run immediately and \
             their output is returned. Any other command is placed in the user's input line; the user runs it \
             and you receive its exit code and output.",
            json!({"type":"object","properties":{
                "command":{"type":"string"},
                "background":{"type":"boolean","description":"start as a background job (for long-running commands)"}
            },"required":["command"]}),
        ),
        t(
            "read_block",
            "Read the n-th most recent command block from the terminal history (1 = the last one): command, exit code, output.",
            json!({"type":"object","properties":{"n":{"type":"integer","minimum":1}},"required":["n"]}),
        ),
        t(
            "jobs",
            "List the shell's current background jobs.",
            json!({"type":"object","properties":{}}),
        ),
        t(
            "job_output",
            "Tail of the log of a background job that was started with run(background=true).",
            json!({"type":"object","properties":{
                "id":{"type":"integer"},"lines":{"type":"integer"}},"required":["id"]}),
        ),
        t(
            "read_file",
            "Read a text file (at most 200 lines / 32 KB per call).",
            json!({"type":"object","properties":{
                "path":{"type":"string"},
                "start_line":{"type":"integer","minimum":1},
                "max_lines":{"type":"integer","minimum":1}},"required":["path"]}),
        ),
    ]
}

/// One `?` question being worked on.
#[derive(Debug, Clone)]
pub struct Turn {
    /// Earlier conversation (before this question).
    pub history: Vec<Message>,
    /// This turn's messages: the question with context, then tool traffic.
    pub msgs: Vec<Message>,
    /// The bare question (what is remembered instead of the context).
    pub q: String,
    pub steps: usize,
}

#[derive(Debug, Clone)]
pub struct Bg {
    pub log: PathBuf,
    pub command: String,
}

/// A turn paused on a command the user has to run.
#[derive(Debug)]
pub struct Pending {
    turn: Turn,
    call_id: String,
    /// The text put into the input line.
    proposed: String,
    bg: Option<Bg>,
    /// Tool result, once the block of the command finished.
    result: Option<String>,
    at: Instant,
}

enum ToolOut {
    /// Result for the model and what to show to the user.
    Done { result: String, shown: String },
    /// Put into the input line; the turn pauses.
    Handoff { line: String, bg: Option<Bg> },
}

struct Env<'a> {
    llm: &'a Llm,
    tracker: &'a Arc<Mutex<BlockTracker>>,
    cancel: &'a AtomicBool,
}

fn cwd_of(tracker: &Arc<Mutex<BlockTracker>>) -> String {
    tracker
        .lock()
        .map(|t| t.cwd().to_owned())
        .ok()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| ".".to_owned())
}

/// Head and tail of `s` within `OUT_CAP` bytes.
fn cap(s: &str) -> String {
    trim_middle(s.trim_end(), OUT_CAP)
}

/// Run an allowlisted command in `cwd`; (text, exit code or None on timeout/cancel).
fn run_readonly(cmd: &str, cwd: &str, cancel: &AtomicBool) -> (String, Option<i32>, bool) {
    use std::os::unix::process::CommandExt;
    let mut c = Command::new("bash");
    c.arg("-c")
        .arg(format!("exec 2>&1; {cmd}"))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("SYSTEMD_PAGER", "")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .process_group(0);
    let mut child = match c.spawn() {
        Ok(c) => c,
        Err(e) => return (format!("cannot run: {e}"), None, false),
    };
    let mut out = child.stdout.take();
    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let b2 = buf.clone();
    let reader = std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Some(Ok(n)) = out.as_mut().map(|o| o.read(&mut chunk)) {
            if n == 0 {
                break;
            }
            if let Ok(mut b) = b2.lock() {
                // keep memory bounded: the head and the tail are all that is shown
                b.extend_from_slice(&chunk[..n]);
                if b.len() > 4 * OUT_CAP {
                    let cut = b.len() - 2 * OUT_CAP;
                    let tail = b.split_off(cut);
                    b.truncate(OUT_CAP);
                    b.extend_from_slice(b"\n[... output omitted ...]\n");
                    b.extend_from_slice(&tail);
                }
            }
        }
    });
    let start = Instant::now();
    let (code, timed_out, stopped) = loop {
        match child.try_wait() {
            Ok(Some(st)) => break (st.code(), false, false),
            Ok(None) => {}
            Err(_) => break (None, false, false),
        }
        let stop = cancel.load(Ordering::Relaxed);
        if stop || start.elapsed() >= RUN_TIMEOUT {
            let pg = nix::unistd::Pid::from_raw(-(child.id() as i32));
            let _ = nix::sys::signal::kill(pg, nix::sys::signal::Signal::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
            break (None, !stop, stop);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // a backgrounded grandchild could keep the pipe open: do not wait for it
    let t0 = Instant::now();
    while !reader.is_finished() && t0.elapsed() < Duration::from_millis(300) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let text = buf
        .lock()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let _ = stopped;
    (text, code, timed_out)
}

fn arg_str(v: &Value, k: &str) -> Option<String> {
    v[k].as_str().map(str::to_owned)
}

fn arg_u(v: &Value, k: &str) -> Option<usize> {
    v[k].as_u64()
        .map(|n| n as usize)
        .or_else(|| v[k].as_str().and_then(|s| s.parse::<usize>().ok()))
}

fn short(cmd: &str, cols: usize) -> String {
    let one: String = cmd
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let max = cols.saturating_sub(24).max(20);
    if one.chars().count() > max {
        let t: String = one.chars().take(max - 1).collect();
        format!("{t}…")
    } else {
        one
    }
}

fn read_block(env: &Env, n: usize) -> String {
    let b = env.tracker.lock().ok().and_then(|t| {
        let bl = t.blocks();
        let len = bl.len();
        (n >= 1 && n <= len)
            .then(|| bl.last(n).next().cloned())
            .flatten()
    });
    match b {
        Some(b) => format!(
            "command: {}\nexit: {}\ncwd: {}\noutput:\n{}",
            b.command,
            b.exit_code.map_or("?".into(), |c| c.to_string()),
            b.cwd,
            cap(&b.output)
        ),
        None => format!("error: no block number {n}"),
    }
}

fn expand(path: &str, cwd: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let p = match path.strip_prefix("~/") {
        Some(r) if !home.is_empty() => PathBuf::from(&home).join(r),
        _ if path == "~" && !home.is_empty() => PathBuf::from(&home),
        _ => PathBuf::from(path),
    };
    if p.is_absolute() {
        p
    } else {
        PathBuf::from(cwd).join(p)
    }
}

/// Lines `start..start+max` of the file at `path` (read-only, capped).
pub fn read_file(path: &str, cwd: &str, start: usize, max: usize) -> Result<String, String> {
    if is_secret_path(path) {
        return Err(format!("refused: {path} looks like a secret file"));
    }
    let p = expand(path, cwd);
    let real = std::fs::canonicalize(&p).map_err(|e| format!("cannot read {path}: {e}"))?;
    if is_secret_path(&real.to_string_lossy()) || is_secret_path(&p.to_string_lossy()) {
        return Err(format!("refused: {path} looks like a secret file"));
    }
    let meta = std::fs::metadata(&real).map_err(|e| format!("cannot read {path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{path} is not a regular file"));
    }
    let mut data = Vec::new();
    std::fs::File::open(&real)
        .and_then(|f| f.take(4 * 1024 * 1024).read_to_end(&mut data))
        .map_err(|e| format!("cannot read {path}: {e}"))?;
    if data[..data.len().min(1024)].contains(&0) {
        return Err(format!("{path} is a binary file"));
    }
    let text = String::from_utf8_lossy(&data);
    let all: Vec<&str> = text.lines().collect();
    let start = start.max(1);
    let max = max.clamp(1, FILE_LINES);
    let mut out = String::new();
    let mut last = start - 1;
    for (i, l) in all.iter().enumerate().skip(start - 1).take(max) {
        if out.len() + l.len() + 1 > FILE_BYTES {
            break;
        }
        out.push_str(l);
        out.push('\n');
        last = i + 1;
    }
    out.push_str(&format!("[lines {start}-{last} of {}]", all.len()));
    Ok(out)
}

fn jobs_dir() -> Option<PathBuf> {
    crate::shell::state_dir().map(|d| d.join("jobs"))
}

/// `~/...` when under $HOME, else a quoted absolute path.
fn shell_path(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().into_owned();
    let home = std::env::var("HOME").unwrap_or_default();
    match s.strip_prefix(&home) {
        Some(r) if !home.is_empty() && r.starts_with('/') && !r.contains(['\'', ' ']) => {
            format!("~{r}")
        }
        _ => format!("'{}'", s.replace('\'', "'\\''")),
    }
}

fn exec_tool(env: &Env, call: &ToolCall, cols: usize) -> ToolOut {
    let v: Value = serde_json::from_str(&call.arguments).unwrap_or_default();
    let done = |result: String, shown: String| ToolOut::Done { result, shown };
    match call.name.as_str() {
        "run" => {
            let Some(cmd) = arg_str(&v, "command").filter(|c| !c.trim().is_empty()) else {
                return done("error: missing command".into(), "run: ?".into());
            };
            let bg = v["background"].as_bool().unwrap_or(false);
            if !bg && is_read_only(&cmd, &env.llm.allow()) {
                let cwd = cwd_of(env.tracker);
                let (out, code, timed_out) = run_readonly(&cmd, &cwd, env.cancel);
                let lines = out.lines().count();
                let tail = if timed_out {
                    " · таймаут 10 с".to_owned()
                } else if code != Some(0) {
                    format!(" · exit {}", code.map_or("?".into(), |c| c.to_string()))
                } else {
                    format!(" · {lines} строк")
                };
                let result = format!(
                    "exit: {}\n{}",
                    if timed_out {
                        "timeout after 10 s".to_owned()
                    } else {
                        code.map_or("?".into(), |c| c.to_string())
                    },
                    cap(&out)
                );
                return done(result, format!("$ {}{tail}", short(&cmd, cols)));
            }
            if unsafe_cmd(&cmd) {
                return done(
                    "error: the command contains control characters and was refused".into(),
                    "команда с управляющими символами отклонена".into(),
                );
            }
            if bg {
                let Some(dir) = jobs_dir() else {
                    return done("error: no state directory".into(), "run: нет HOME".into());
                };
                let _ = std::fs::create_dir_all(&dir);
                let n = env.llm.jobs.lock().map_or(0, |mut j| {
                    j.seq += 1;
                    j.seq
                });
                // unique across sessions too
                let n = format!("{}-{n}", std::process::id());
                let log = dir.join(format!("{n}.log"));
                let line = format!("{} &>{} &", cmd.trim_end(), shell_path(&log));
                return ToolOut::Handoff {
                    line,
                    bg: Some(Bg { log, command: cmd }),
                };
            }
            ToolOut::Handoff {
                line: cmd,
                bg: None,
            }
        }
        "read_block" => {
            let n = arg_u(&v, "n").unwrap_or(1);
            done(read_block(env, n), format!("read_block {n}"))
        }
        "jobs" => {
            let d = env
                .llm
                .jobs
                .lock()
                .map(|j| j.describe())
                .unwrap_or_default();
            done(d, "jobs".into())
        }
        "job_output" => {
            let id = arg_u(&v, "id").unwrap_or(0) as u32;
            let lines = arg_u(&v, "lines").unwrap_or(40).clamp(1, 500);
            let log = env.llm.jobs.lock().ok().and_then(|j| j.log_of(id));
            let r = match log {
                None => format!(
                    "error: job {id} has no log (only jobs started with run(background=true) do)"
                ),
                Some(p) => match std::fs::read_to_string(&p) {
                    Ok(s) => {
                        let all: Vec<&str> = s.lines().collect();
                        let from = all.len().saturating_sub(lines);
                        cap(&all[from..].join("\n"))
                    }
                    Err(e) => format!("error: cannot read the log: {e}"),
                },
            };
            done(r, format!("job_output {id}"))
        }
        "read_file" => {
            let Some(path) = arg_str(&v, "path") else {
                return done("error: missing path".into(), "read_file: ?".into());
            };
            let cwd = cwd_of(env.tracker);
            let start = arg_u(&v, "start_line").unwrap_or(1);
            let max = arg_u(&v, "max_lines").unwrap_or(FILE_LINES);
            let (r, tail) = match read_file(&path, &cwd, start, max) {
                Ok(s) => {
                    let n = s.lines().count().saturating_sub(1);
                    (s, format!(" · {n} строк"))
                }
                Err(e) => (format!("error: {e}"), " · отказ".to_owned()),
            };
            done(r, format!("read_file {}{tail}", short(&path, cols)))
        }
        other => done(
            format!("error: unknown tool {other}"),
            format!("неизвестный инструмент {other}"),
        ),
    }
}

/// The agent loop for one turn. Returns the text for the input line.
pub fn drive(
    llm: &Llm,
    tracker: &Arc<Mutex<BlockTracker>>,
    t: &Target,
    mut turn: Turn,
    task: &Task,
) -> Option<String> {
    let mut use_tools = t.tools;
    let env = Env {
        llm,
        tracker,
        cancel: &task.shared.cancel,
    };
    loop {
        if task.cancelled() {
            note_only(task, "⏹");
            return None;
        }
        let mut all = turn.history.clone();
        all.extend(turn.msgs.iter().cloned());
        let req = Request {
            max_tokens: 2048,
            tools: if use_tools { tool_defs() } else { Vec::new() },
            ..Request::chat(
                if use_tools {
                    format!("{SYSTEM}{AGENT_GUIDE}")
                } else {
                    SYSTEM.to_owned()
                },
                masked_history(&all, t.local),
            )
        };
        let mut r = Renderer::new(&task.theme, task.cols);
        let mut full = String::new();
        let res = llmsh_llm::stream_tools(&t.ep, &req, &task.shared.cancel, |Event::Delta(d)| {
            full.push_str(&d);
            let mut out = Vec::new();
            r.push(&d, &mut out);
            if !out.is_empty() {
                task.write(&out);
            }
        });
        let mut out = Vec::new();
        let (usage, calls) = match res {
            Ok(x) => x,
            Err(Error::Cancelled) => {
                r.note("⏹", &mut out);
                task.write(&out);
                return None;
            }
            Err(Error::Http {
                status: 400,
                message,
            }) if use_tools && {
                let m = message.to_lowercase();
                m.contains("tool") || m.contains("function")
            } =>
            {
                tracing::warn!("model rejects tools: {message}");
                llm.mark_no_tools(&t.label);
                use_tools = false;
                continue;
            }
            Err(e) => {
                tracing::warn!("ask: {e}");
                r.finish(&mut out, false);
                r.note(&short_reason(&e, t), &mut out);
                task.write(&out);
                return None;
            }
        };
        if calls.is_empty() {
            // A cut-off answer never yields a command to run.
            let cmd = r.finish(&mut out, !usage.truncated);
            if usage.truncated {
                r.note("ответ оборван (лимит токенов)", &mut out);
            }
            task.write(&out);
            turn.msgs.push(Message::assistant(full));
            remember_turn(llm, &turn);
            return cmd;
        }
        r.finish(&mut out, false);
        task.write(&out);
        turn.msgs.push(Message::assistant_calls(&full, &calls));
        let mut paused: Option<(String, String, Option<Bg>)> = None;
        for c in &calls {
            if paused.is_some() {
                turn.msgs.push(Message::tool_result(
                    &c.id,
                    "not executed: one command at a time; the previous one waits for the user",
                ));
                continue;
            }
            if task.cancelled() {
                note_only(task, "⏹");
                return None;
            }
            turn.steps += 1;
            if turn.steps > MAX_STEPS {
                note_only(task, &format!("лимит шагов ({MAX_STEPS})"));
                return None;
            }
            match exec_tool(&env, c, task.cols as usize) {
                ToolOut::Done { result, shown } => {
                    if task.cancelled() {
                        note_only(task, "⏹");
                        return None;
                    }
                    note_only(task, &shown);
                    turn.msgs.push(Message::tool_result(&c.id, result));
                }
                ToolOut::Handoff { line, bg } => paused = Some((c.id.clone(), line, bg)),
            }
        }
        if let Some((id, line, bg)) = paused {
            if let Ok(mut p) = llm.pending.lock() {
                *p = Some(Pending {
                    turn,
                    call_id: id,
                    proposed: line.clone(),
                    bg,
                    result: None,
                    at: Instant::now(),
                });
            }
            return Some(line);
        }
    }
}

fn note_only(task: &Task, text: &str) {
    let r = Renderer::new(&task.theme, task.cols);
    let mut out = Vec::new();
    r.note(text, &mut out);
    task.write(&out);
}

/// A block finished: is it the command a paused turn waits for?
pub fn on_block(llm: &Llm, block: &Block) {
    let Ok(mut g) = llm.pending.lock() else {
        return;
    };
    let Some(p) = g.as_mut() else { return };
    if p.result.is_some() {
        return;
    }
    if p.at.elapsed() > PENDING_TTL {
        *g = None;
        return;
    }
    let ran = block.command.trim();
    let same = ran == p.proposed.trim();
    let first = |s: &str| s.split_whitespace().next().unwrap_or("").to_owned();
    if !same && first(ran) != first(&p.proposed) {
        *g = None; // the user did something else: the agent stops silently
        return;
    }
    let code = block.exit_code.map_or("?".into(), |c| c.to_string());
    let mut res = String::new();
    if !same {
        res.push_str(&format!(
            "The user edited the command before running it. What they actually ran: {ran}\n"
        ));
    }
    if let Some(bg) = &p.bg {
        let id = block
            .output
            .trim_start()
            .strip_prefix('[')
            .and_then(|r| r.split(']').next())
            .and_then(|n| n.parse::<u32>().ok());
        res.push_str(&format!(
            "Started in the background (exit {code}); output goes to {}.",
            bg.log.display()
        ));
        if let Some(id) = id {
            res.push_str(&format!(
                " Job number: {id}: use job_output({id}) and jobs()."
            ));
            if let Ok(mut j) = llm.jobs.lock() {
                j.link(id, bg.log.clone());
            }
        }
    } else {
        res.push_str(&format!("exit: {code}\noutput:\n{}", cap(&block.output)));
    }
    p.result = Some(res);
}

/// Is a paused turn ready to continue?
pub fn resumable(llm: &Llm) -> bool {
    llm.pending
        .lock()
        .is_ok_and(|p| p.as_ref().is_some_and(|p| p.result.is_some()))
}

/// Continue a paused turn (runs on the worker thread).
pub fn resume(llm: &Llm, tracker: &Arc<Mutex<BlockTracker>>, task: &Task) -> Option<String> {
    let p = llm.pending.lock().ok()?.take()?;
    let t = llm.target("chat")?;
    let mut turn = p.turn;
    turn.msgs.push(Message::tool_result(
        p.call_id,
        p.result.unwrap_or_default(),
    ));
    drive(llm, tracker, &t, turn, task)
}

/// Start a turn for a question.
pub fn start_turn(history: Vec<Message>, user_msg: String, q: String) -> Turn {
    Turn {
        history,
        msgs: vec![Message::user(user_msg)],
        q,
        steps: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_file_refuses_secrets_and_caps() {
        let d = tempfile::TempDir::new().unwrap();
        let cwd = d.path().to_str().unwrap();
        std::fs::write(d.path().join(".env"), "TOKEN=x\n").unwrap();
        std::fs::write(d.path().join("k.pem"), "KEY\n").unwrap();
        std::fs::create_dir(d.path().join(".ssh")).unwrap();
        std::fs::write(d.path().join(".ssh/id"), "k").unwrap();
        std::fs::write(d.path().join("link"), "x").unwrap();
        std::os::unix::fs::symlink(d.path().join(".env"), d.path().join("sneaky")).unwrap();
        for p in [
            ".env",
            "k.pem",
            ".ssh/id",
            "sneaky",
            "~/.ssh/id_rsa",
            "~/.netrc",
        ] {
            let e = read_file(p, cwd, 1, 10).unwrap_err();
            assert!(
                e.contains("refused") || e.contains("cannot read"),
                "{p}: {e}"
            );
        }
        assert!(read_file("sneaky", cwd, 1, 10)
            .unwrap_err()
            .contains("refused"));
        assert_eq!(
            read_file("link", cwd, 1, 10).unwrap(),
            "x\n[lines 1-1 of 1]"
        );
        let big: String = (1..=500).map(|i| format!("line{i}\n")).collect();
        std::fs::write(d.path().join("big"), big).unwrap();
        let r = read_file("big", cwd, 10, 1000).unwrap();
        assert!(
            r.starts_with("line10\n") && r.ends_with("[lines 10-209 of 500]"),
            "{r}"
        );
        std::fs::write(d.path().join("bin"), [0u8, 1, 2]).unwrap();
        assert!(read_file("bin", cwd, 1, 5).unwrap_err().contains("binary"));
    }

    #[test]
    fn readonly_runner_captures_output_and_times_out() {
        let c = AtomicBool::new(false);
        let (o, code, t) = run_readonly("ls / | head -1; ls /nonexistent-x", "/", &c);
        assert!(!t && code != Some(0), "{o}");
        assert!(o.contains("nonexistent-x"), "stderr merged: {o}");
        let (o, code, _) = run_readonly("pwd", "/tmp", &c);
        assert_eq!((o.trim(), code), ("/tmp", Some(0)));
        let stop = AtomicBool::new(true);
        let t0 = Instant::now();
        let (_, code, _) = run_readonly("sleep 5", "/", &stop);
        assert!(code.is_none() && t0.elapsed() < Duration::from_secs(2));
    }
}
