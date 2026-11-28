//! LLM features under a real pty against an in-process mock server.

use std::process::Command;
use std::time::{Duration, Instant};

use aish_llm::mock::{self, Mock};
use expectrl::{Eof, Session};
use tempfile::TempDir;

fn config(url: &str) -> String {
    format!(
        "providers:\n  - id: mock\n    url: {url}\n    type: chat\n    models: {{m: {{id: test}}}}\nroles:\n  chat: {{id: mock/m}}\n  complete: {{id: mock/m}}\n"
    )
}

fn base(home: &TempDir) -> Command {
    std::fs::write(home.path().join(".bashrc"), "PS1='E> '\n").unwrap();
    std::fs::write(home.path().join("hist"), "").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aish"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.path())
        .env("HISTFILE", home.path().join("hist"))
        .env("TERM", "xterm")
        .env("XDG_STATE_HOME", home.path().join("state"))
        // never reach the real release API from tests (refused at once)
        .env("AISH_TEST_UPDATE_URL", "http://127.0.0.1:9/");
    // ... and in release mode (override ignored) the daily check is not due
    let st = home.path().join("state/aish");
    std::fs::create_dir_all(&st).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(st.join("update-check"), format!("{now}\n\n{now}\n")).unwrap();
    cmd
}

fn spawn(cmd: Command) -> Session {
    let mut p = Session::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(Duration::from_secs(10)));
    p
}

fn with_config(home: &TempDir, mock: &Mock) -> Session {
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), config(&mock.url())).unwrap();
    spawn(base(home))
}

fn settle() {
    std::thread::sleep(Duration::from_millis(400));
}

fn answering_mock() -> Mock {
    Mock::start(|r, c| {
        if r.path.ends_with("/models") {
            mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#);
        } else {
            mock::chat_stream(
                c,
                &[
                    "Hello **bold** and `code`\nTry:\n```ba",
                    "sh\necho llm-ran\n```\n",
                ],
                Duration::from_millis(20),
            );
        }
    })
}

#[test]
fn question_streams_styled_answer_and_prefills_command() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    let t = Instant::now();
    p.send("? say hi\r").unwrap();
    let f = p.expect("Hello").unwrap();
    let before = String::from_utf8_lossy(f.before()).into_owned();
    assert!(before.ends_with("\x1b[2m│ \x1b[0m"), "{before:?}");
    eprintln!("enter -> first answer row: {:?}", t.elapsed());
    p.expect("\x1b[1mbold\x1b[0m").unwrap();
    p.expect("\x1b[36mcode\x1b[0m").unwrap();
    p.expect("Try:").unwrap();
    p.expect("E> ").unwrap();
    // the fenced block is not printed in the stream; it is in the input line
    let f = p.expect("echo llm-ran").unwrap();
    assert!(!String::from_utf8_lossy(f.before()).contains("llm-ran"));
    settle();
    p.send("\r").unwrap();
    p.expect("\x07llm-ran\r\n").unwrap();
    p.expect("E> ").unwrap();
    // context reached the model, conversation is a single user turn
    let q = m
        .requests()
        .into_iter()
        .find(|r| r.path.ends_with("/chat/completions"))
        .unwrap();
    assert!(
        q.body.contains("say hi") && q.body.contains("cwd:"),
        "{}",
        q.body
    );
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn ctrl_c_cancels_stream_and_returns_to_prompt() {
    let home = TempDir::new().unwrap();
    let m = Mock::start(|_, c| {
        mock::sse_head(c);
        mock::sse_data(c, &mock::chat_chunk("first line\n"));
        std::thread::sleep(Duration::from_secs(5));
    });
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("? slow\r").unwrap();
    p.expect("first line").unwrap();
    p.send("\x03").unwrap();
    p.expect("⏹").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("echo fine\r").unwrap();
    p.expect("\x07fine\r\n").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn http_error_is_one_dim_line() {
    let home = TempDir::new().unwrap();
    let m = Mock::start(|_, c| mock::json(c, 401, r#"{"error":{"message":"no"}}"#));
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("?\r").unwrap();
    p.expect("│ ").unwrap();
    p.expect("401 from mock").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn aish_status_switch_and_config_prefill() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("aish status\r").unwrap();
    p.expect("chat     mock/m · test · ok ").unwrap();
    p.expect("complete").unwrap();
    p.expect("suggestions: on").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("aish off\r").unwrap();
    p.expect("suggestions: off").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("aish chat mock/m\r").unwrap();
    p.expect("chat → mock/m").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("aish config\r").unwrap();
    p.expect("E> ").unwrap();
    p.expect("config.yaml").unwrap();
    p.send("\x03").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn first_run_probe_writes_config_and_says_so_once() {
    let home = TempDir::new().unwrap();
    let m = Mock::start(|_, c| {
        mock::json(
            c,
            200,
            r#"{"data":[{"id":"qwen3:32b"},{"id":"qwen2.5-coder:1.5b"},{"id":"nomic-embed-text"}]}"#,
        )
    });
    let mut cmd = base(&home);
    cmd.env("AISH_PROBE_OLLAMA", m.url())
        .env("AISH_PROBE_LMSTUDIO", "http://127.0.0.1:1/v1");
    let mut p = spawn(cmd);
    p.expect(expectrl::Regex(
        // (the line is cut to the 80-column pty, the rest is not asserted)
        "aish · chat: ollama/qwen3:32b · complete: ollama/qwen2.5-coder:1.5b · ",
    ))
    .unwrap();
    p.expect("E> ").unwrap();
    settle();
    let cfg = std::fs::read_to_string(home.path().join(".config/aish/config.yaml")).unwrap();
    assert!(
        cfg.contains("qwen2.5-coder:1.5b") && cfg.contains("roles:"),
        "{cfg}"
    );
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn first_run_with_nothing_found_writes_nothing() {
    let home = TempDir::new().unwrap();
    let mut cmd = base(&home);
    cmd.env("AISH_PROBE_OLLAMA", "http://127.0.0.1:1/v1")
        .env("AISH_PROBE_LMSTUDIO", "http://127.0.0.1:1/v1");
    let mut p = spawn(cmd);
    p.expect("E> ").unwrap();
    settle();
    assert!(!home.path().join(".config/aish/config.yaml").exists());
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn cli_subcommands_and_bash_passthrough() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), config(&m.url())).unwrap();
    let run = |args: &[&str]| {
        let mut c = base(&home);
        c.args(args).stdin(std::process::Stdio::null());
        c.output().unwrap()
    };
    let o = run(&["status"]);
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("chat     mock/m · test · ok") && s.contains("suggestions"),
        "{s}"
    );
    let o = run(&["config"]);
    assert!(String::from_utf8_lossy(&o.stdout)
        .trim()
        .ends_with("aish/config.yaml"));
    assert!(String::from_utf8_lossy(&run(&["--version"]).stdout).starts_with("aish "));
    // everything else is bash
    let o = run(&["-c", "echo via-bash"]);
    assert_eq!(o.stdout, b"via-bash\n");
}

fn suggest_mock() -> Mock {
    Mock::start(|r, c| {
        if r.path.ends_with("/models") {
            return mock::json(c, 200, r#"{"data":[]}"#);
        }
        if r.body.contains("echo fo\",\"role\"") {
            mock::chat_stream(c, &["o-bar"], Duration::ZERO);
        } else if r.body.contains("git che\",\"role\"") {
            std::thread::sleep(Duration::from_millis(700));
            mock::chat_stream(c, &["ckout-OLD"], Duration::ZERO);
        } else if r.body.contains("git chex\",\"role\"") {
            mock::chat_stream(c, &["git chex-NEW"], Duration::ZERO); // repeats the line
        } else {
            mock::chat_stream(c, &[""], Duration::ZERO);
        }
    })
}

#[test]
fn model_ghost_appears_after_debounce_and_is_accepted() {
    let home = TempDir::new().unwrap();
    let m = suggest_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("echo fo").unwrap();
    let t = Instant::now();
    p.expect("o-bar").unwrap();
    eprintln!("last key -> ghost on screen: {:?}", t.elapsed());
    assert!(t.elapsed() >= Duration::from_millis(140));
    p.send("\x1b[C").unwrap(); // accept
    p.send("\r").unwrap();
    p.expect("\x07foo-bar\r\n").unwrap();
    let q = m
        .requests()
        .into_iter()
        .find(|r| r.path.ends_with("/chat/completions"))
        .unwrap();
    assert!(
        q.body.contains("\"max_tokens\":48") && q.body.contains("\"temperature\":0.0"),
        "{}",
        q.body
    );
    assert!(
        q.body.contains("cwd:") && q.body.contains("files:"),
        "{}",
        q.body
    );
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn stale_model_suggestion_is_discarded() {
    let home = TempDir::new().unwrap();
    let m = suggest_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("git che").unwrap();
    std::thread::sleep(Duration::from_millis(350)); // request for "git che" is in flight
    p.send("x").unwrap();
    let f = p.expect("-NEW").unwrap();
    assert!(!String::from_utf8_lossy(f.before()).contains("OLD"));
    std::thread::sleep(Duration::from_millis(800)); // the old answer would land now
    p.send("\x03").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn aish_off_silences_suggestions() {
    let home = TempDir::new().unwrap();
    let m = suggest_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("aish off\r").unwrap();
    p.expect("off").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("echo fo").unwrap();
    std::thread::sleep(Duration::from_millis(600));
    p.send("\r").unwrap();
    p.expect("\x07fo\r\n").unwrap();
    assert!(m
        .requests()
        .iter()
        .all(|r| !r.path.ends_with("/chat/completions")));
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn enter_pressed_while_streaming_never_runs_the_model_command() {
    let home = TempDir::new().unwrap();
    let m = Mock::start(|_, c| {
        mock::chat_stream(
            c,
            &["Hello\n", "there\n", "```bash\necho llm-ran\n```\n"],
            Duration::from_millis(300),
        )
    });
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("? hi\r").unwrap();
    p.expect("Hello").unwrap();
    p.send("\r").unwrap(); // Enter mid-stream, "to confirm" something not yet seen
    p.expect("E> ").unwrap();
    p.expect("echo llm-ran").unwrap(); // pre-filled...
    p.set_expect_timeout(Some(Duration::from_millis(1200)));
    assert!(
        p.expect("\x07llm-ran").is_err(),
        "the command ran unreviewed"
    );
    p.set_expect_timeout(Some(Duration::from_secs(10)));
    p.send("\x03").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn first_run_finds_llama_server() {
    let home = TempDir::new().unwrap();
    let m = Mock::start(|_, c| mock::json(c, 200, r#"{"data":[{"id":"gguf-model-7b"}]}"#));
    let mut cmd = base(&home);
    cmd.env("AISH_PROBE_OLLAMA", "http://127.0.0.1:1/v1")
        .env("AISH_PROBE_LMSTUDIO", "http://127.0.0.1:1/v1")
        .env("AISH_PROBE_LLAMACPP", m.url());
    let mut p = spawn(cmd);
    p.expect("chat: llamacpp/gguf-model-7b").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn ai_is_the_short_command_when_the_shell_has_no_ai() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("ai status\r").unwrap();
    p.expect("chat     mock/m · test · ok ").unwrap();
    p.expect("suggestions: on").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("ai off\r").unwrap();
    p.expect("suggestions: off").unwrap();
    p.expect("E> ").unwrap();
    settle();
    // the long form always works
    p.send("aish on\r").unwrap();
    p.expect("suggestions: on").unwrap();
    p.expect("E> ").unwrap();
    settle();
    // Tab completes subcommands of both
    p.send("ai conf\t").unwrap();
    p.expect("config").unwrap();
    p.send("\x03").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

fn with_command(home: &TempDir, m: &Mock, command: &str) -> Session {
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        format!("command: {command}\n{}", config(&m.url())),
    )
    .unwrap();
    spawn(base(home))
}

#[test]
fn ai_wins_even_over_an_executable_of_that_name() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let ai = bin.join("ai");
    std::fs::write(&ai, "#!/bin/sh\necho real-ai-ran\n").unwrap();
    std::fs::set_permissions(&ai, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), config(&m.url())).unwrap();
    let mut cmd = base(&home);
    cmd.env(
        "PATH",
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
    );
    let mut p = spawn(cmd);
    p.expect("E> ").unwrap();
    settle();
    p.send("ai status\r").unwrap();
    p.expect("chat     mock/m").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn custom_command_name_works_and_ai_is_left_to_bash() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let mut p = with_command(&home, &m, "mm");
    p.expect("E> ").unwrap();
    settle();
    p.send("mm status\r").unwrap();
    p.expect("chat     mock/m · test · ok ").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("ai\r").unwrap();
    p.expect("ai: command not found").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn command_off_leaves_ai_to_bash_but_aish_works() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let mut p = with_command(&home, &m, "off");
    p.expect("E> ").unwrap();
    settle();
    p.send("ai\r").unwrap();
    p.expect("ai: command not found").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("aish status\r").unwrap();
    p.expect("chat     mock/m · test · ok ").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn quiet_marker_is_drawn_at_the_right_edge_and_not_left_in_scrollback() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("echo hi").unwrap();
    // default pty width is 80 columns: hint at 1-based column 79
    p.expect("\x1b[79G\x1b[0;2m?\x1b[0m").unwrap();
    p.send("\r").unwrap();
    let f = p.expect("\x07hi\r\n").unwrap();
    // the submitted line is redrawn without the marker, erased before bash gets it
    let tail = String::from_utf8_lossy(f.before()).into_owned();
    let after_erase = tail.rsplit("\x1b[J").next().unwrap_or("");
    assert!(
        after_erase.contains("echo hi") && !after_erase.contains("\x1b[79G"),
        "{tail:?}"
    );
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

// ---- agent: tools -------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// No `complete` role: ghost suggestions would add requests to the count.
fn config_kind(url: &str, kind: &str) -> String {
    // (`run: off`: the model's commands are prefilled, as these tests expect)
    format!("{}run: off\n", config_kind_run(url, kind))
}

fn config_kind_run(url: &str, kind: &str) -> String {
    config(url)
        .replace("type: chat", &format!("type: {kind}"))
        .replace("  complete: {id: mock/m}\n", "")
}

fn with_kind(home: &TempDir, m: &Mock, kind: &str) -> Session {
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), config_kind(&m.url(), kind)).unwrap();
    spawn(base(home))
}

/// Answer by step: `steps[i]` is `(text, calls)` for the i-th request; the
/// last one repeats.
type Calls = Vec<(&'static str, &'static str, String)>;

fn stepped(kind: &'static str, steps: Vec<(&'static str, Calls)>) -> Mock {
    let n = Arc::new(AtomicUsize::new(0));
    Mock::start(move |_, c| {
        let i = n.fetch_add(1, Ordering::SeqCst).min(steps.len() - 1);
        let (text, calls) = &steps[i];
        let calls: Vec<(&str, &str, &str)> =
            calls.iter().map(|(a, b, d)| (*a, *b, d.as_str())).collect();
        if calls.is_empty() {
            if kind == "chat" {
                mock::chat_stream(c, &[text], Duration::ZERO)
            } else {
                mock::resp_stream(c, &[text], Duration::ZERO)
            }
        } else if kind == "chat" {
            mock::chat_calls_stream(c, text, &calls)
        } else {
            mock::resp_calls_stream(c, text, &calls)
        }
    })
}

fn run_call(id: &'static str, cmd: &str) -> Calls {
    vec![(id, "run", serde_json::json!({ "command": cmd }).to_string())]
}

fn agent_steps() -> Vec<(&'static str, Calls)> {
    vec![
        ("Смотрю. ", run_call("c1", "ls Cargo.toml")),
        ("Собираю. ", run_call("c2", "echo built")),
        ("Готово: all-done\n", vec![]),
    ]
}

fn agent_flow(kind: &'static str) {
    let home = TempDir::new().unwrap();
    let m = stepped(kind, agent_steps());
    let mut p = with_kind(&home, &m, kind);
    p.expect("E> ").unwrap();
    settle();
    p.send("? собери проект\r").unwrap();
    // read-only command ran by aish itself: dim line, no output shown
    p.expect("$ ls Cargo.toml · 1 lines").unwrap();
    // the mutating one waits in the input line
    p.expect("E> ").unwrap();
    p.expect("echo built").unwrap();
    settle();
    assert_eq!(m.requests().len(), 2);
    p.send("\r").unwrap();
    p.expect("built").unwrap();
    // the loop continues by itself and streams under a new section
    p.expect("all-done").unwrap();
    p.expect("E> ").unwrap();
    let reqs = m.requests();
    assert_eq!(reqs.len(), 3);
    let first = &reqs[0].body;
    assert!(first.contains("\"tools\""), "{first}");
    let last = &reqs[2].body;
    assert!(last.contains("Cargo.toml"), "read-only result sent");
    assert!(last.contains("exit: 0") && last.contains("built"), "{last}");
    assert!(!last.contains("edited"), "{last}");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn agent_runs_readonly_then_hands_off_and_continues_chat() {
    agent_flow("chat");
}

#[test]
fn agent_runs_readonly_then_hands_off_and_continues_responses() {
    agent_flow("responses");
}

#[test]
fn agent_edited_command_is_reported_as_edited() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", agent_steps());
    let mut p = with_kind(&home, &m, "chat");
    p.expect("E> ").unwrap();
    settle();
    p.send("? собери\r").unwrap();
    p.expect("echo built").unwrap();
    settle();
    p.send(" now\r").unwrap();
    p.expect("all-done").unwrap();
    let last = m.requests()[2].body.clone();
    assert!(last.contains("edited the command"), "{last}");
    assert!(last.contains("echo built now"), "{last}");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn agent_stops_silently_when_the_line_is_abandoned() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", agent_steps());
    let mut p = with_kind(&home, &m, "chat");
    p.expect("E> ").unwrap();
    settle();
    p.send("? собери\r").unwrap();
    p.expect("echo built").unwrap();
    settle();
    p.send("\x03").unwrap(); // clear the line
    p.expect("E> ").unwrap();
    settle();
    p.send("printf 'other\\n'\r").unwrap();
    p.expect("other\r\n").unwrap();
    p.expect("E> ").unwrap();
    settle();
    assert_eq!(m.requests().len(), 2, "no continuation request");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn agent_ctrl_c_stops_it() {
    let home = TempDir::new().unwrap();
    let n = Arc::new(AtomicUsize::new(0));
    let m = Mock::start(move |_, c| {
        if n.fetch_add(1, Ordering::SeqCst) == 0 {
            mock::chat_calls_stream(c, "", &[("c1", "run", r#"{"command":"ls Cargo.toml"}"#)]);
        } else {
            mock::sse_head(c);
            mock::sse_data(c, &mock::chat_chunk("thinking\n"));
            std::thread::sleep(Duration::from_secs(5));
        }
    });
    let mut p = with_kind(&home, &m, "chat");
    p.expect("E> ").unwrap();
    settle();
    p.send("? x\r").unwrap();
    p.expect("thinking").unwrap();
    p.send("\x03").unwrap();
    p.expect("⏹").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn agent_step_cap() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", vec![("", run_call("c", "pwd"))]);
    let mut p = with_kind(&home, &m, "chat");
    p.expect("E> ").unwrap();
    settle();
    p.send("? loop\r").unwrap();
    p.expect("step limit (12)").unwrap();
    p.expect("E> ").unwrap();
    assert_eq!(m.requests().len(), 13);
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn tools_rejected_with_400_falls_back_once_and_is_remembered() {
    let home = TempDir::new().unwrap();
    let m = Mock::start(|r, c| {
        if r.body.contains("\"tools\"") {
            mock::json(
                c,
                400,
                r#"{"error":{"message":"tools are not supported by this model"}}"#,
            );
        } else {
            mock::chat_stream(
                c,
                &["Вот:\n```bash\necho fallback-ran\n```\n"],
                Duration::ZERO,
            );
        }
    });
    let mut p = with_kind(&home, &m, "chat");
    p.expect("E> ").unwrap();
    settle();
    p.send("? q\r").unwrap();
    p.expect("E> ").unwrap();
    p.expect("echo fallback-ran").unwrap();
    assert_eq!(m.requests().len(), 2);
    p.send("\x03").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("? again\r").unwrap();
    p.expect("echo fallback-ran").unwrap();
    // remembered: the next question did not try tools again
    assert_eq!(m.requests().len(), 3);
    p.send("\x03").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn tools_off_in_config_sends_no_tools() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        config_kind(&m.url(), "chat").replace("{id: test}", "{id: test, tools: off}"),
    )
    .unwrap();
    let mut p = spawn(base(&home));
    p.expect("E> ").unwrap();
    settle();
    p.send("? hi\r").unwrap();
    p.expect("echo llm-ran").unwrap();
    assert!(!m.requests()[0].body.contains("\"tools\""));
    p.send("\x03").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

// ---- background jobs -----------------------------------------------------

fn plain_home() -> (TempDir, Session) {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let p = with_config(&home, &m);
    std::mem::forget(m);
    (home, p)
}

#[test]
fn finished_background_job_gets_one_line_and_bash_stays_quiet() {
    let (_h, mut p) = plain_home();
    p.expect("E> ").unwrap();
    settle();
    p.send("sleep 1 &\r").unwrap();
    p.expect("E> ").unwrap();
    settle();
    // the hint counts running jobs
    p.send("x").unwrap();
    p.expect("[1] ?").unwrap();
    p.send("\x03").unwrap();
    // idle at the prompt until it ends
    let f = p
        .expect(expectrl::Regex(r"✓.*\[1\] sleep 1 · [0-9]+s"))
        .unwrap();
    let before = String::from_utf8_lossy(f.before()).into_owned();
    assert!(!before.contains("Done"), "bash line leaked: {before:?}");
    p.expect("E> ").unwrap();
    settle();
    p.send("jobs\r").unwrap();
    let f = p.expect("E> ").unwrap();
    let out = String::from_utf8_lossy(f.before()).into_owned();
    assert!(!out.contains("sleep"), "table is empty: {out:?}");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn failed_background_job_shows_exit_code() {
    let (_h, mut p) = plain_home();
    p.expect("E> ").unwrap();
    settle();
    p.send("sh -c 'sleep 1; exit 3' &\r").unwrap();
    p.expect("E> ").unwrap();
    p.expect(expectrl::Regex(r"✗.*\[1\] sh -c .* · [0-9]+s · exit 3"))
        .unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

// ---- ghost text for our own command --------------------------------------

#[test]
fn ai_arguments_get_instant_local_ghost_text() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("ai ").unwrap();
    p.expect("status").unwrap(); // ghost, dim
    p.expect("model status").unwrap(); // transient description
    p.send("c").unwrap(); // `ai c` -> `hat`
    p.expect("hat").unwrap();
    p.send("\x03").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("ai chat ").unwrap(); // only one model: the current one, nothing to suggest
    settle();
    p.send("\x03").unwrap();
    p.expect("E> ").unwrap();
    settle();
    // typing `ai o` while suggestions are on: the toggle that does something
    p.send("ai o").unwrap();
    p.expect("ff").unwrap();
    p.send("\x03").unwrap();
    p.expect("E> ").unwrap();
    settle();
    // no model request was made for any of those lines
    assert!(
        m.requests().iter().all(|r| !r.body.contains("\"ai ")),
        "{:?}",
        m.requests()
            .iter()
            .map(|r| r.body.clone())
            .collect::<Vec<_>>()
    );
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn clearing_the_proposed_line_ends_the_turn_even_if_the_same_command_is_typed_later() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", agent_steps());
    let mut p = with_kind(&home, &m, "chat");
    p.expect("E> ").unwrap();
    settle();
    p.send("? собери\r").unwrap();
    p.expect("echo built").unwrap();
    settle();
    p.send("\x03").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("echo built\r").unwrap();
    p.expect("built\r\n").unwrap();
    p.expect("E> ").unwrap();
    settle();
    assert_eq!(m.requests().len(), 2, "no continuation request");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn a_different_command_with_the_same_program_does_not_continue_the_turn() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", agent_steps());
    let mut p = with_kind(&home, &m, "chat");
    p.expect("E> ").unwrap();
    settle();
    p.send("? собери\r").unwrap();
    p.expect("echo built").unwrap();
    settle();
    // replace the whole line (Ctrl-A, Ctrl-K) with another echo and run it
    p.send("\x01\x0becho something-else-entirely-different\r")
        .unwrap();
    p.expect("something-else-entirely-different\r\n").unwrap();
    p.expect("E> ").unwrap();
    settle();
    assert_eq!(m.requests().len(), 2, "no continuation request");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

// ---- aish init ----------------------------------------------------------

fn init_cmd(home: &TempDir, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_aish"));
    c.arg("init")
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.path())
        // hermetic detection: nothing listens there
        .env("AISH_PROBE_OLLAMA", "http://127.0.0.1:9/v1")
        .env("AISH_PROBE_LMSTUDIO", "http://127.0.0.1:9/v1")
        .env("AISH_PROBE_LLAMACPP", "http://127.0.0.1:9/v1")
        .stdin(std::process::Stdio::null());
    for (k, v) in env {
        c.env(k, v);
    }
    c
}

fn run_init(mut c: Command) -> (i32, String, String) {
    let o = c.output().unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

fn deepseek_mock() -> Mock {
    Mock::start(|_, c| {
        mock::json(
            c,
            200,
            r#"{"data":[{"id":"deepseek-flash"},{"id":"deepseek-v4-pro"}]}"#,
        )
    })
}

#[test]
fn init_without_a_terminal_changes_nothing_and_fails() {
    let home = TempDir::new().unwrap();
    let m = deepseek_mock();
    std::fs::write(home.path().join(".bashrc"), "export A=1\n").unwrap();
    let (code, out, err) = run_init(init_cmd(
        &home,
        &[],
        &[
            ("DEEPSEEK_API_KEY", "sk-x"),
            ("AISH_DEEPSEEK_URL", &m.url()),
        ],
    ));
    assert_eq!(code, 1, "{out}{err}");
    assert!(
        out.contains(".bashrc") && out.contains("+"),
        "shows what it would do: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join(".bashrc")).unwrap(),
        "export A=1\n"
    );
    assert!(!home.path().join(".config/aish/config.yaml").exists());
}

#[test]
fn init_is_idempotent_backs_up_once_and_removes_cleanly() {
    let home = TempDir::new().unwrap();
    let bashrc = home.path().join(".bashrc");
    std::fs::write(&bashrc, "export A=1\nalias x=y\n").unwrap();
    let (code, out, err) = run_init(init_cmd(&home, &["--yes"], &[]));
    assert_eq!(code, 0, "{out}{err}");
    let once = std::fs::read_to_string(&bashrc).unwrap();
    assert_eq!(once.matches("# >>> aish >>>").count(), 1);
    assert!(
        once.starts_with("# >>> aish >>>")
            && once.contains("exec '/")
            && once.contains("-z ${AISH-}")
    );
    let bak = home.path().join(".bashrc.aish-bak");
    assert_eq!(
        std::fs::read_to_string(&bak).unwrap(),
        "export A=1\nalias x=y\n"
    );
    let (code, out, _) = run_init(init_cmd(&home, &["--yes"], &[]));
    assert_eq!(code, 0);
    assert!(out.contains("already set up"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&bashrc).unwrap(),
        once,
        "second run changes nothing"
    );
    assert_eq!(
        std::fs::read_to_string(&bak).unwrap(),
        "export A=1\nalias x=y\n",
        "backup is the original"
    );
    let (code, out, _) = run_init(init_cmd(&home, &["--remove"], &[]));
    assert_eq!(code, 0);
    assert!(out.contains(".bashrc") && out.contains("removed"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&bashrc).unwrap(),
        "export A=1\nalias x=y\n"
    );
    assert!(bak.exists(), "backups are kept");
    let (_, out, _) = run_init(init_cmd(&home, &["--remove"], &[]));
    assert!(out.contains("no block"), "{out}");
}

#[test]
fn init_keeps_an_existing_config() {
    let home = TempDir::new().unwrap();
    let m = deepseek_mock();
    let cfg = home.path().join(".config/aish/config.yaml");
    std::fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    std::fs::write(&cfg, "providers: []\n").unwrap();
    let (code, out, _) = run_init(init_cmd(
        &home,
        &["--yes"],
        &[
            ("DEEPSEEK_API_KEY", "sk-x"),
            ("AISH_DEEPSEEK_URL", &m.url()),
        ],
    ));
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("config exists"), "{out}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "providers: []\n");
}

#[test]
fn init_writes_env_keys_as_references_never_literally() {
    let home = TempDir::new().unwrap();
    let m = deepseek_mock();
    let (code, out, _) = run_init(init_cmd(
        &home,
        &["--yes"],
        &[
            ("DEEPSEEK_API_KEY", "sk-very-secret-value"),
            ("AISH_DEEPSEEK_URL", &m.url()),
        ],
    ));
    assert_eq!(code, 0, "{out}");
    let cfg = std::fs::read_to_string(home.path().join(".config/aish/config.yaml")).unwrap();
    assert!(cfg.contains("$DEEPSEEK_API_KEY"), "{cfg}");
    assert!(!cfg.contains("sk-very-secret-value"), "{cfg}");
    assert!(
        cfg.contains("deepseek-flash") && cfg.contains("deepseek/"),
        "{cfg}"
    );
    assert!(!out.contains("sk-very-secret-value"));
    // the request carried the key, the file does not
    assert!(m.requests()[0]
        .header("authorization")
        .is_some_and(|h| h.contains("sk-very")));
}

#[test]
fn bashrc_block_starts_aish_without_a_loop_and_has_an_escape_hatch() {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_aish"), bin.join("aish")).unwrap();
    std::fs::write(home.path().join(".bashrc"), "PS1='E> '\n").unwrap();
    let (code, out, err) = run_init(init_cmd(&home, &["--yes"], &[]));
    assert_eq!(code, 0, "{out}{err}");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let shell = |disable: bool| {
        let mut c = Command::new("bash");
        c.arg("-i")
            .env_clear()
            .env("PATH", &path)
            .env("HOME", home.path())
            .env("HISTFILE", home.path().join("hist"))
            .env("XDG_STATE_HOME", home.path().join("state"))
            .env("TERM", "xterm");
        if disable {
            c.env("AISH_DISABLE", "1");
        }
        spawn(c)
    };
    // exec'd into aish (its OSC marks appear), once: no loop, exit ends it
    let mut p = shell(false);
    let f = p.expect("E> ").unwrap();
    assert!(
        String::from_utf8_lossy(f.before()).contains("133;A"),
        "aish is running"
    );
    settle();
    p.send("echo ok-$((6*7))\r").unwrap();
    p.expect("ok-42").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
    // the escape hatch: plain bash
    let mut p = shell(true);
    let f = p.expect("E> ").unwrap();
    assert!(
        !String::from_utf8_lossy(f.before()).contains("133;"),
        "plain bash"
    );
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn the_completion_sidecar_sourcing_bashrc_is_not_replaced_by_aish() {
    let home = TempDir::new().unwrap();
    std::fs::write(home.path().join(".bashrc"), "PS1='E> '\n").unwrap();
    let (code, _, _) = run_init(init_cmd(&home, &["--yes"], &[]));
    assert_eq!(code, 0);
    // like the sidecar: stdin is not a terminal, `exec` is a no-op function, AISH unset
    let out = Command::new("bash")
        .args([
            "-c",
            "exec() { return 0; }; set -i; . ~/.bashrc </dev/null; echo survived",
        ])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "survived");
}

#[test]
fn failed_start_falls_back_to_a_usable_bash_without_looping() {
    let home = TempDir::new().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_aish"), bin.join("aish")).unwrap();
    std::fs::write(home.path().join(".bashrc"), "PS1='E> '\n").unwrap();
    let (code, out, err) = run_init(init_cmd(&home, &["--yes"], &[]));
    assert_eq!(code, 0, "{out}{err}");
    let mut c = Command::new("bash");
    c.arg("-i")
        .env_clear()
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("HOME", home.path())
        .env("HISTFILE", home.path().join("hist"))
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("TERM", "xterm")
        // every aish start fails; the block would exec aish forever without the guard
        .env("AISH_TEST_FAIL_START", "1");
    let mut p = spawn(c);
    p.expect("E> ").unwrap();
    p.send("echo ok-$((6*7)) $AISH_DISABLE\r").unwrap();
    p.expect("ok-42 1").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

// ---- Ctrl-Q overlay (ask while a command runs) ----

fn overlay_mock(answer: &'static str) -> Mock {
    Mock::start(move |r, c| {
        if r.path.ends_with("/models") {
            mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#);
        } else {
            mock::chat_stream(c, &[answer], Duration::from_millis(10));
        }
    })
}

fn chat_body(m: &Mock) -> String {
    m.requests()
        .into_iter()
        .find(|r| r.body.contains("A program is running"))
        .map(|r| r.body)
        .unwrap_or_default()
}

#[test]
fn overlay_asks_shows_command_and_inserts_it_without_newline() {
    let home = TempDir::new().unwrap();
    let m = overlay_mock("Try this:\n```bash\nINSERTED-TEXT\n```\n");
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("cat\r").unwrap();
    settle();
    p.send("\x11").unwrap(); // Ctrl-Q while cat runs
    let f = p.expect("no output").unwrap(); // dim context line
    assert!(
        String::from_utf8_lossy(f.before()).contains("\x1b[?1049h"),
        "alternate screen entered"
    );
    p.send("why\r").unwrap();
    p.expect("Try this:").unwrap();
    p.expect("Enter — insert · Esc — close").unwrap();
    // the model got the context but no local tools
    let body = chat_body(&m);
    assert!(
        body.contains("running command: cat") && body.contains("read_block"),
        "{body}"
    );
    assert!(
        !body.contains("\"read_file\"") && !body.contains("\"name\":\"run\""),
        "{body}"
    );
    p.send("\r").unwrap(); // insert
    let f = p.expect("INSERTED-TEXT").unwrap(); // echoed by the tty after the overlay closed
    assert!(String::from_utf8_lossy(f.before()).contains("\x1b[?1049l"));
    settle();
    // Ctrl-D flushes cat's partial line; a newline would have sent it already
    p.send("\x04").unwrap();
    let f = p.expect("INSERTED-TEXT").unwrap();
    // cat's copy follows the echo on the same line: no newline was typed
    assert!(!String::from_utf8_lossy(f.before()).contains('\n'));
    p.send("\x04").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn overlay_holds_back_output_until_it_closes() {
    let home = TempDir::new().unwrap();
    let m = overlay_mock("ok\n");
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("sleep 1.2; echo LATE-OUTPUT-XYZ; cat\r").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    p.send("\x11").unwrap();
    p.expect("no output").unwrap();
    std::thread::sleep(Duration::from_millis(1800)); // the echo happens meanwhile
    p.send("\x1b").unwrap(); // Esc closes
    let f = p.expect("LATE-OUTPUT-XYZ").unwrap();
    let before = String::from_utf8_lossy(f.before()).into_owned();
    assert!(
        before.contains("\x1b[?1049l"),
        "shown only after close: {before:?}"
    );
    p.send("\x04").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn double_ctrl_q_sends_a_literal_ctrl_q() {
    let home = TempDir::new().unwrap();
    let m = overlay_mock("ok\n");
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("stty -ixon; cat -v\r").unwrap();
    settle();
    p.send("\x11").unwrap();
    p.expect("no output").unwrap();
    p.send("\x11").unwrap(); // empty input: close and pass the key on
    p.expect("\x1b[?1049l").unwrap();
    settle();
    p.send("\r").unwrap();
    p.expect("^Q").unwrap();
    p.send("\x04").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn overlay_multiline_command_is_not_offered_without_bracketed_paste() {
    let home = TempDir::new().unwrap();
    let m = overlay_mock("Two steps:\n```bash\nLINE-ONE \\\n  LINE-TWO\n```\n");
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("cat\r").unwrap();
    settle();
    p.send("\x11").unwrap();
    p.expect("no output").unwrap();
    p.send("\r").unwrap(); // empty Enter = bare `?`
    p.expect("LINE-ONE").unwrap(); // shown
    let f = p.expect("Esc — close").unwrap();
    assert!(!String::from_utf8_lossy(f.before()).contains("insert"));
    p.send("\r").unwrap(); // nothing to insert: Enter does nothing
    std::thread::sleep(Duration::from_millis(200));
    p.send("\x1b").unwrap();
    p.expect("\x1b[?1049l").unwrap();
    p.send("\x04").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn overlay_over_an_alternate_screen_app_redraws_it_by_resizing() {
    let home = TempDir::new().unwrap();
    let m = overlay_mock("ok\n");
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    // a "full-screen" app: enters the alternate screen, reports window changes
    p.send(
        "bash -c 'trap \"echo WINCH-$((3+4))\" WINCH; printf \"\\033[?1049h\"; echo APP-$((20+2)); read -t 20 x'\r",
    )
    .unwrap();
    p.expect("APP-22").unwrap();
    settle();
    p.send("\x11").unwrap();
    let f = p.expect("full-screen app").unwrap();
    let before = String::from_utf8_lossy(f.before()).into_owned();
    assert!(!before.contains("\x1b[?1049h"), "already in it: {before:?}");
    p.send("\x1b").unwrap();
    p.expect("WINCH-7").unwrap(); // the size was nudged on close
    p.send("\r").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

// ---- web tools ----

fn web_reply(c: &mut std::net::TcpStream, ct: &str, body: &str) {
    use std::io::Write;
    let _ = write!(
        c,
        "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

#[test]
fn agent_searches_then_reads_a_page_through_mock_backends() {
    let home = TempDir::new().unwrap();
    let ddg = Mock::start(|_, c| {
        web_reply(
            c,
            "text/html",
            r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.test%2Fx">Docs X</a>
<a class="result__snippet" href="x">about X</a>"#,
        )
    });
    let page = Mock::start(|_, c| {
        web_reply(
            c,
            "text/html",
            "<html><body><h1>Page head</h1><p>PAGE-BODY-MARK paragraph with enough words to \
             be counted as real readable content of the page, more than the near-empty limit \
             of the extractor so no fallback is needed.</p></body></html>",
        )
    });
    let read_args = serde_json::json!({"url": format!("http://127.0.0.1:{}/doc", page.port)});
    let m = stepped(
        "chat",
        vec![
            (
                "",
                vec![("w1", "web_search", r#"{"query":"how to X"}"#.to_owned())],
            ),
            ("", vec![("w2", "web_read", read_args.to_string())]),
            ("Готово: web-done\n", vec![]),
        ],
    );
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), config_kind(&m.url(), "chat")).unwrap();
    let mut cmd = base(&home);
    cmd.env(
        "AISH_TEST_WEB_DDG",
        format!("http://127.0.0.1:{}/html/", ddg.port),
    )
    .env(
        "AISH_TEST_WEB_ALLOW_PRIVATE",
        format!("127.0.0.1:{}", page.port),
    );
    let mut p = spawn(cmd);
    p.expect("E> ").unwrap();
    settle();
    p.send("? как сделать X\r").unwrap();
    p.expect("↗ search: how to X · duckduckgo").unwrap();
    p.expect(format!("↗ 127.0.0.1:{}/doc · direct", page.port))
        .unwrap();
    p.expect("web-done").unwrap();
    p.expect("E> ").unwrap();
    let reqs = m.requests();
    assert_eq!(reqs.len(), 3);
    assert!(reqs[0].body.contains("\"web_search\"") && reqs[0].body.contains("\"web_read\""));
    assert!(
        reqs[1].body.contains("https://docs.test/x"),
        "{}",
        reqs[1].body
    );
    assert!(reqs[2].body.contains("PAGE-BODY-MARK") && reqs[2].body.contains("not instructions"));
    assert!(
        ddg.requests()[0].body.contains("q=how+to+X"),
        "{}",
        ddg.requests()[0].body
    );
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn web_read_of_a_private_address_fails_with_one_dim_line() {
    let home = TempDir::new().unwrap();
    let m = stepped(
        "chat",
        vec![
            (
                "",
                vec![(
                    "w1",
                    "web_read",
                    r#"{"url":"http://127.0.0.1:9/"}"#.to_owned(),
                )],
            ),
            ("конец\n", vec![]),
        ],
    );
    let mut p = with_kind(&home, &m, "chat");
    p.expect("E> ").unwrap();
    settle();
    p.send("? открой\r").unwrap();
    p.expect("↗ failed: direct: blocked").unwrap();
    p.expect("конец").unwrap();
    p.expect("E> ").unwrap();
    assert!(m.requests()[1].body.contains("error: direct: blocked"));
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn native_search_model_gets_the_builtin_tool_and_no_web_search_function() {
    let home = TempDir::new().unwrap();
    let m = Mock::start(|_, c| {
        mock::sse_head(c);
        mock::sse_data(
            c,
            r#"{"type":"response.output_item.done","item":{"type":"web_search_call","action":{"query":"native q"}}}"#,
        );
        mock::sse_data(c, &mock::resp_delta("found-it\n"));
        mock::sse_data(
            c,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        );
    });
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = config_kind(&m.url(), "responses").replace("{id: test}", "{id: test, web: native}");
    std::fs::write(dir.join("config.yaml"), cfg).unwrap();
    let mut p = spawn(base(&home));
    p.expect("E> ").unwrap();
    settle();
    p.send("? новости\r").unwrap();
    p.expect("found-it").unwrap();
    p.expect("↗ search: native q · provider").unwrap();
    p.expect("E> ").unwrap();
    let b = m.requests()[0].body.clone();
    assert!(b.contains(r#"{"type":"web_search"}"#), "{b}");
    assert!(!b.contains(r#""name":"web_search""#), "{b}");
    assert!(b.contains(r#""name":"web_read""#), "{b}");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn web_off_sends_no_web_tools_and_overlay_keeps_web_but_not_run() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", vec![("ok\n", vec![])]);
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        format!("{}web: off\n", config_kind(&m.url(), "chat")),
    )
    .unwrap();
    let mut p = spawn(base(&home));
    p.expect("E> ").unwrap();
    settle();
    p.send("? привет\r").unwrap();
    p.expect("ok").unwrap();
    p.expect("E> ").unwrap();
    let b = m.requests()[0].body.clone();
    assert!(b.contains("\"read_file\"") && !b.contains("web_"), "{b}");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();

    // overlay: web tools are host independent
    let home = TempDir::new().unwrap();
    let m = overlay_mock("Fine.\n");
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("cat\r").unwrap();
    settle();
    p.send("\x11").unwrap();
    p.expect("no output").unwrap();
    p.send("why\r").unwrap();
    p.expect("Fine.").unwrap();
    let body = chat_body(&m);
    assert!(
        body.contains("\"web_search\"") && body.contains("\"web_read\""),
        "{body}"
    );
    assert!(!body.contains("\"read_file\""), "{body}");
    settle();
    p.send("\x1b").unwrap();
    settle();
    p.send("\x04").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn native_search_rejection_retries_without_it_keeping_function_tools() {
    let home = TempDir::new().unwrap();
    let m = Mock::start(|r, c| {
        if r.body.contains(r#""type":"web_search""#) {
            mock::json(
                c,
                400,
                r#"{"error":{"message":"tool web_search is not supported"}}"#,
            );
        } else {
            mock::resp_stream(c, &["plain-ok\n"], Duration::ZERO);
        }
    });
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = config_kind(&m.url(), "responses").replace("{id: test}", "{id: test, web: native}");
    std::fs::write(dir.join("config.yaml"), cfg).unwrap();
    let mut p = spawn(base(&home));
    p.expect("E> ").unwrap();
    settle();
    p.send("? привет\r").unwrap();
    p.expect("plain-ok").unwrap();
    p.expect("E> ").unwrap();
    let r = m.requests();
    assert_eq!(r.len(), 2);
    assert!(
        r[1].body.contains(r#""name":"run""#) && r[1].body.contains(r#""name":"web_search""#),
        "{}",
        r[1].body
    );
    assert!(!r[1].body.contains(r#""type":"web_search""#));
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn native_search_needs_provider_in_the_search_chain() {
    let home = TempDir::new().unwrap();
    let m = Mock::start(|_, c| mock::resp_stream(c, &["ok\n"], Duration::ZERO));
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = config_kind(&m.url(), "responses").replace("{id: test}", "{id: test, web: native}");
    std::fs::write(
        dir.join("config.yaml"),
        format!("{cfg}web: {{search: [duckduckgo]}}\n"),
    )
    .unwrap();
    let mut p = spawn(base(&home));
    p.expect("E> ").unwrap();
    settle();
    p.send("? привет\r").unwrap();
    p.expect("ok").unwrap();
    p.expect("E> ").unwrap();
    let b = m.requests()[0].body.clone();
    assert!(
        !b.contains(r#""type":"web_search""#) && b.contains(r#""name":"web_search""#),
        "{b}"
    );
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

// ---- the model runs commands (run: ask | auto | off) and tools on/off ------

/// Default config (run mode unset = ask) plus `extra` lines.
fn with_run(home: &TempDir, m: &Mock, extra: &str) -> Session {
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        format!("{}{extra}", config_kind_run(&m.url(), "chat")),
    )
    .unwrap();
    spawn(base(home))
}

fn cfg_text(home: &TempDir) -> String {
    std::fs::read_to_string(home.path().join(".config/aish/config.yaml")).unwrap()
}

/// `cmds` one after another, then the final answer.
fn run_steps(cmds: &[&str]) -> Vec<(&'static str, Calls)> {
    let ids = ["c1", "c2", "c3"];
    let mut v: Vec<(&'static str, Calls)> = cmds
        .iter()
        .enumerate()
        .map(|(i, c)| ("", run_call(ids[i], c)))
        .collect();
    v.push(("Готово: all-done\n", vec![]));
    v
}

const PRINT: &str = "basename out-42";

#[test]
fn run_once_types_the_command_and_the_model_gets_exit_and_output() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", run_steps(&[PRINT]));
    let mut p = with_run(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    p.expect("▶ ").unwrap();
    p.expect("basename").unwrap();
    p.expect("Enter run · s session · a always · Esc no")
        .unwrap();
    assert_eq!(m.requests().len(), 1, "waits for the user");
    settle();
    p.send("\r").unwrap();
    // typed into bash like by the user: after the prompt, visible
    p.expect("E> ").unwrap();
    p.expect("basename").unwrap();
    p.expect("out-42\r\n").unwrap();
    p.expect("all-done").unwrap();
    p.expect("E> ").unwrap();
    let reqs = m.requests();
    assert_eq!(reqs.len(), 2);
    let b = &reqs[1].body;
    assert!(b.contains("exit: 0") && b.contains("out-42"), "{b}");
    assert!(b.contains("duration: "), "{b}");
    // not remembered as always
    assert!(!cfg_text(&home).contains("run:"));
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn esc_declines_and_the_model_continues() {
    let home = TempDir::new().unwrap();
    let marker = home.path().join("declined-marker");
    let cmd = format!("touch {}", marker.display());
    let m = stepped("chat", run_steps(&[&cmd]));
    let mut p = with_run(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    p.expect("▶ ").unwrap();
    p.expect("touch").unwrap();
    settle();
    p.send("\x1b").unwrap();
    p.expect("declined").unwrap();
    p.expect("all-done").unwrap();
    p.expect("E> ").unwrap();
    let reqs = m.requests();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[1].body.contains("declined"), "{}", reqs[1].body);
    assert!(!marker.exists(), "a declined command must not run");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn s_allows_the_session_and_the_next_command_runs_without_asking() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", run_steps(&["basename one-1", "basename two-2"]));
    let mut p = with_run(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    p.expect("Enter run").unwrap();
    settle();
    p.send("s").unwrap();
    p.expect("one-1\r\n").unwrap();
    // no prompt for the second one: its output just arrives
    let c = p.expect("two-2\r\n").unwrap();
    let before = String::from_utf8_lossy(c.before()).into_owned();
    assert!(!before.contains("Enter run"), "asked again: {before}");
    p.expect("all-done").unwrap();
    p.expect("E> ").unwrap();
    assert_eq!(m.requests().len(), 3);
    // session only
    assert!(!cfg_text(&home).contains("run:"));
    settle();
    p.send_line("ai run").unwrap();
    p.expect("run: auto").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn a_persists_run_auto_in_the_config() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", run_steps(&[PRINT]));
    let mut p = with_run(&home, &m, "# my note\n");
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    p.expect("Enter run").unwrap();
    settle();
    p.send("a").unwrap();
    p.expect("out-42\r\n").unwrap();
    p.expect("all-done").unwrap();
    p.expect("E> ").unwrap();
    let c = cfg_text(&home);
    assert!(
        c.contains("\nrun: auto\n") && c.contains("# my note"),
        "{c}"
    );
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn hard_list_commands_ask_even_in_auto() {
    let home = TempDir::new().unwrap();
    let marker = home.path().join("victim");
    std::fs::create_dir(&marker).unwrap();
    let cmd = format!("rm -rf {}", marker.display());
    let m = stepped("chat", run_steps(&[&cmd]));
    let mut p = with_run(&home, &m, "run: auto\n");
    p.expect("E> ").unwrap();
    settle();
    p.send("? clean up\r").unwrap();
    p.expect("▶ ").unwrap();
    p.expect("rm -rf").unwrap();
    // only the choices that make sense: auto is already on
    p.expect("Enter run · Esc no").unwrap();
    settle();
    assert!(marker.exists());
    settle();
    p.send("\x1b").unwrap();
    p.expect("all-done").unwrap();
    p.expect("E> ").unwrap();
    assert!(marker.exists(), "declined: still there");
    assert!(m.requests()[1].body.contains("declined"));
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn auto_runs_a_harmless_command_without_asking() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", run_steps(&[PRINT]));
    let mut p = with_run(&home, &m, "run: auto\n");
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    let c = p.expect("out-42\r\n").unwrap();
    assert!(!String::from_utf8_lossy(c.before()).contains("Enter run"));
    p.expect("all-done").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn an_alias_or_function_named_like_a_plain_program_makes_it_ask() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", run_steps(&[PRINT]));
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        format!("{}run: auto\n", config_kind_run(&m.url(), "chat")),
    )
    .unwrap();
    let cmd = base(&home);
    std::fs::write(
        home.path().join(".bashrc"),
        "PS1='E> '\nbasename() { command basename \"$@\"; }\n",
    )
    .unwrap();
    let mut p = spawn(cmd);
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    p.expect("Enter run · Esc no").unwrap();
    settle();
    p.send("\x1b").unwrap();
    p.expect("all-done").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn a_line_bash_never_started_is_reported_not_taken_for_the_next_command() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", run_steps(&["echo 'unclosed"]));
    let mut p = with_run(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    p.expect("Enter run").unwrap();
    settle();
    p.send("\r").unwrap();
    // bash waits for the closing quote; the user gives up
    p.expect("> ").unwrap();
    settle();
    p.send("\x03").unwrap();
    p.expect("all-done").unwrap();
    let b = m.requests()[1].body.clone();
    assert!(b.contains("did not run"), "{b}");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn injection_is_refused_when_the_line_is_not_empty() {
    let home = TempDir::new().unwrap();
    let marker = home.path().join("must-not-run");
    let cmd = format!("touch {}", marker.display());
    let steps = run_steps(&[&cmd]);
    let n = Arc::new(AtomicUsize::new(0));
    let m = Mock::start(move |_, c| {
        let i = n.fetch_add(1, Ordering::SeqCst);
        if i == 0 {
            // time to type while the model "thinks"
            std::thread::sleep(Duration::from_millis(700));
            let (text, calls) = &steps[0];
            let calls: Vec<(&str, &str, &str)> =
                calls.iter().map(|(a, b, d)| (*a, *b, d.as_str())).collect();
            mock::chat_calls_stream(c, text, &calls);
        } else {
            mock::chat_stream(c, &["all-done\n"], Duration::ZERO);
        }
    });
    let mut p = with_run(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    p.send("xy").unwrap();
    p.expect("▶ ").unwrap();
    p.expect("touch").unwrap();
    settle();
    p.send("\r").unwrap();
    p.expect("all-done").unwrap();
    p.expect("E> ").unwrap();
    let reqs = m.requests();
    assert!(
        reqs[1].body.contains("input line is not empty"),
        "{}",
        reqs[1].body
    );
    assert!(!marker.exists());
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn ctrl_c_on_the_running_command_is_reported_as_interrupted() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", run_steps(&["tail -f /dev/null"]));
    let mut p = with_run(&home, &m, "run: auto\n");
    p.expect("E> ").unwrap();
    settle();
    p.send("? wait\r").unwrap();
    p.expect("E> ").unwrap();
    p.expect("tail -f /dev/null").unwrap();
    std::thread::sleep(Duration::from_millis(600));
    p.send("\x03").unwrap();
    p.expect("all-done").unwrap();
    let b = m.requests()[1].body.clone();
    assert!(b.contains("interrupted by the user"), "{b}");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn ai_run_off_prefills_instead_of_running() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", run_steps(&[PRINT]));
    let mut p = with_run(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send_line("ai run").unwrap();
    p.expect("run: ask").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send_line("ai run off").unwrap();
    p.expect("run: off · this session").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    let c = p.expect("E> ").unwrap();
    assert!(!String::from_utf8_lossy(c.before()).contains('▶'));
    settle();
    assert_eq!(m.requests().len(), 1, "waits for Enter in the line");
    settle();
    p.send("\r").unwrap();
    p.expect("out-42").unwrap();
    p.expect("all-done").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn ai_run_save_persists_and_ai_tools_lists() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", vec![("ok\n", vec![])]);
    let mut p = with_run(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send_line("ai run auto --save").unwrap();
    p.expect("run: auto · saved").unwrap();
    assert!(cfg_text(&home).contains("\nrun: auto\n"));
    p.expect("E> ").unwrap();
    settle();
    p.send_line("ai tools files off --save").unwrap();
    p.expect("files: off · saved").unwrap();
    assert!(
        cfg_text(&home).contains("tools:\n  files: off\n"),
        "{}",
        cfg_text(&home)
    );
    p.expect("E> ").unwrap();
    settle();
    p.send_line("ai tools").unwrap();
    p.expect("files off").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn ai_tools_web_off_removes_web_tools_from_the_request() {
    let home = TempDir::new().unwrap();
    let m = stepped("chat", vec![("ok\n", vec![])]);
    let mut p = with_run(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? one\r").unwrap();
    p.expect("ok").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send_line("ai tools web off").unwrap();
    p.expect("web: off · this session").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("? two\r").unwrap();
    p.expect("E> ").unwrap();
    settle();
    let reqs = m.requests();
    assert!(reqs[0].body.contains("web_search"), "{}", reqs[0].body);
    let b = &reqs[1].body;
    assert!(b.contains("\"run\"") && b.contains("read_file"), "{b}");
    assert!(!b.contains("web_search") && !b.contains("web_read"), "{b}");
    // nothing about it in the system prompt either
    assert!(!b.contains("Web:"), "{b}");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn a_call_to_a_turned_off_tool_is_refused_and_files_off_hides_read_file() {
    let home = TempDir::new().unwrap();
    let m = stepped(
        "chat",
        vec![
            (
                "",
                vec![("c1", "web_search", r#"{"query":"x"}"#.to_owned())],
            ),
            ("fine\n", vec![]),
        ],
    );
    let mut p = with_run(&home, &m, "tools: {web: off, files: off, run: off}\n");
    p.expect("E> ").unwrap();
    settle();
    p.send("? look\r").unwrap();
    p.expect("fine").unwrap();
    p.expect("E> ").unwrap();
    let reqs = m.requests();
    let b = &reqs[0].body;
    assert!(
        !b.contains("web_") && !b.contains("read_file") && !b.contains("\"run\""),
        "{b}"
    );
    assert!(b.contains("read_block"), "{b}");
    assert!(!b.contains("run with a read-only command"), "{b}");
    assert!(
        reqs[1].body.contains("turned off by the user"),
        "{}",
        reqs[1].body
    );
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

// ---- prefix, conversation commands, compaction -------------------------

/// Plain answers; a summary request (it says "Write the notes now") gets
/// `SUMSUM` bullets.
fn convo_mock() -> Mock {
    Mock::start(|r, c| {
        if r.path.ends_with("/models") {
            mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#);
        } else if r.body.contains("Write the notes now") {
            mock::chat_stream(c, &["- SUMSUM goal\n"], Duration::ZERO);
        } else {
            mock::chat_stream(c, &["Hello there\n"], Duration::ZERO);
        }
    })
}

fn with_yaml(home: &TempDir, yaml: &str) -> Session {
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), yaml).unwrap();
    spawn(base(home))
}

fn chat_bodies(m: &Mock) -> Vec<String> {
    m.requests()
        .into_iter()
        .filter(|r| r.path.ends_with("/chat/completions") && r.body.contains("Question: "))
        .map(|r| r.body)
        .collect()
}

fn ask_and_wait(p: &mut Session, line: &str) {
    p.send(format!("{line}\r")).unwrap();
    p.expect("Hello there").unwrap();
    p.expect("E> ").unwrap();
    settle();
}

fn window_config(url: &str, window: u32) -> String {
    format!(
        "providers:\n  - id: mock\n    url: {url}\n    type: chat\n    models: {{m: {{id: test, context_window: {window}, tools: off}}}}\nroles:\n  chat: {{id: mock/m}}\n  complete: {{id: mock/m}}\n"
    )
}

#[test]
fn comma_prefix_asks_and_question_mark_is_a_command_again() {
    let home = TempDir::new().unwrap();
    let m = convo_mock();
    let mut p = with_yaml(&home, &format!("prefix: ','\n{}", config(&m.url())));
    p.expect("E> ").unwrap();
    settle();
    p.send(", say hi\r").unwrap();
    p.expect("Hello there").unwrap();
    p.expect("E> ").unwrap();
    settle();
    assert!(chat_bodies(&m)[0].contains("say hi"));
    // `?` is bash's again: nothing reaches the model
    p.send("? say hi\r").unwrap();
    p.expect("command not found").unwrap();
    p.expect("E> ").unwrap();
    settle();
    assert_eq!(chat_bodies(&m).len(), 1);
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn new_conversation_forgets_the_turns() {
    let home = TempDir::new().unwrap();
    let m = convo_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    ask_and_wait(&mut p, "? first-question");
    ask_and_wait(&mut p, "? second-question");
    assert!(chat_bodies(&m)[1].contains("first-question"));
    p.send("?n\r").unwrap();
    p.expect("◇ new conversation").unwrap();
    p.expect("E> ").unwrap();
    settle();
    ask_and_wait(&mut p, "? third-question");
    let b = chat_bodies(&m);
    assert!(b.len() == 3 && b[2].contains("third-question"));
    assert!(
        !b[2].contains("first-question") && !b[2].contains("second-question"),
        "{}",
        b[2]
    );
    // the old spelling still works
    p.send("?!\r").unwrap();
    p.expect("◇ new conversation").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn compact_now_replaces_the_turns_with_the_summary() {
    let home = TempDir::new().unwrap();
    let m = convo_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    ask_and_wait(&mut p, "? first-question");
    p.send("?c\r").unwrap();
    p.expect("◇ compacted").unwrap();
    p.expect("tokens").unwrap();
    p.expect("E> ").unwrap();
    settle();
    let all = m.requests();
    let sum = all
        .iter()
        .find(|r| r.body.contains("Write the notes now"))
        .expect("a summary request");
    assert!(sum.body.contains("first-question"));
    ask_and_wait(&mut p, "? next-question");
    let b = chat_bodies(&m);
    let last = b.last().unwrap();
    assert!(
        last.contains("SUMSUM")
            && last.contains("next-question")
            && !last.contains("first-question"),
        "{last}"
    );
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn compact_with_nothing_to_compact_says_so() {
    let home = TempDir::new().unwrap();
    let m = convo_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("?c\r").unwrap();
    p.expect("◇ nothing to compact").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn small_window_triggers_auto_compaction_before_the_next_answer() {
    let home = TempDir::new().unwrap();
    let m = convo_mock();
    let mut p = with_yaml(&home, &window_config(&m.url(), 4096));
    p.expect("E> ").unwrap();
    settle();
    // ~2000 tokens: with the context and prompt over 70% of 4096 once remembered
    let long = format!("? first-question {}", "word ".repeat(1600));
    ask_and_wait(&mut p, &long);
    assert!(!m
        .requests()
        .iter()
        .any(|r| r.body.contains("Write the notes now")));
    p.send("? next-question\r").unwrap();
    p.expect("◇ compacted").unwrap();
    p.expect("Hello there").unwrap();
    p.expect("E> ").unwrap();
    settle();
    let b = chat_bodies(&m);
    let last = b.last().unwrap();
    assert!(
        last.contains("SUMSUM") && !last.contains("first-question"),
        "{last}"
    );
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn failed_compaction_drops_old_turns_and_says_so() {
    let home = TempDir::new().unwrap();
    let m = Mock::start(|r, c| {
        if r.path.ends_with("/models") {
            mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#);
        } else if r.body.contains("Write the notes now") {
            mock::json(c, 500, r#"{"error":{"message":"boom"}}"#);
        } else {
            mock::chat_stream(c, &["Hello there\n"], Duration::ZERO);
        }
    });
    let mut p = with_yaml(&home, &window_config(&m.url(), 4096));
    p.expect("E> ").unwrap();
    settle();
    ask_and_wait(&mut p, &format!("? first-question {}", "word ".repeat(800)));
    ask_and_wait(
        &mut p,
        &format!("? second-question {}", "word ".repeat(800)),
    );
    p.send("? third-question\r").unwrap();
    p.expect("dropped old turns").unwrap();
    p.expect("Hello there").unwrap();
    p.expect("E> ").unwrap();
    settle();
    let b = chat_bodies(&m);
    assert!(!b.last().unwrap().contains("first-question"));
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn status_shows_the_context_window() {
    let home = TempDir::new().unwrap();
    let m = convo_mock();
    let mut p = with_yaml(&home, &window_config(&m.url(), 32768));
    p.expect("E> ").unwrap();
    settle();
    p.send_line("aish status").unwrap();
    p.expect("ctx 32k").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn llama_props_gives_the_window() {
    let home = TempDir::new().unwrap();
    let m = Mock::start(|r, c| {
        if r.path.ends_with("/props") {
            mock::json(c, 200, r#"{"default_generation_settings":{"n_ctx":16384}}"#);
        } else {
            mock::json(c, 200, r#"{"data":[{"id":"test"}]}"#);
        }
    });
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send_line("aish status").unwrap();
    p.expect("ctx 16k").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

fn max_tokens_of(body: &str) -> u64 {
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    v["max_tokens"]
        .as_u64()
        .or_else(|| v["max_completion_tokens"].as_u64())
        .unwrap()
}

#[test]
fn summary_is_capped_and_a_local_summariser_gets_unmasked_text_only_when_all_are_local() {
    // all local: the summariser reads the text as it is; the cap is 2048
    let home = TempDir::new().unwrap();
    let m = convo_mock();
    let cfg = window_config(&m.url(), 65536);
    let mut p = with_yaml(&home, &cfg);
    p.expect("E> ").unwrap();
    settle();
    ask_and_wait(&mut p, "? export API_KEY=abc123secret why");
    p.send("?c\r").unwrap();
    p.expect("◇ compacted").unwrap();
    p.expect("E> ").unwrap();
    settle();
    let sum = m
        .requests()
        .into_iter()
        .find(|r| r.body.contains("Write the notes now"))
        .unwrap();
    assert!(sum.body.contains("abc123secret"));
    assert_eq!(max_tokens_of(&sum.body), 2048);
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();

    // a non-local fallback in the chat role: the summary may go there, so mask
    let home = TempDir::new().unwrap();
    let m = convo_mock();
    let cfg = format!(
        "providers:\n  - id: mock\n    url: {}\n    type: chat\n    models: {{m: {{id: test}}}}\n  - id: cloud\n    url: http://cloud.invalid/v1\n    type: chat\n    models: {{c: {{id: big}}}}\nroles:\n  chat: {{id: [mock/m, cloud/c]}}\n  complete: {{id: mock/m}}\n",
        m.url()
    );
    let mut p = with_yaml(&home, &cfg);
    p.expect("E> ").unwrap();
    settle();
    ask_and_wait(&mut p, "? export API_KEY=abc123secret why");
    p.send("?c\r").unwrap();
    p.expect("◇ compacted").unwrap();
    p.expect("E> ").unwrap();
    settle();
    let sum = m
        .requests()
        .into_iter()
        .find(|r| r.body.contains("Write the notes now"))
        .unwrap();
    assert!(!sum.body.contains("abc123secret"), "{}", sum.body);
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn typing_words_at_the_prompt_never_decides() {
    let home = TempDir::new().unwrap();
    let marker = home.path().join("typed-marker");
    let cmd = format!("touch {}", marker.display());
    let m = stepped("chat", run_steps(&[&cmd]));
    let mut p = with_run(&home, &m, "# note\n");
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    p.expect("Enter run").unwrap();
    settle();
    for word in ["yes", "nah", "ok, go ahead"] {
        p.send(word).unwrap();
        p.expect("press Enter, s, a or Esc").unwrap();
        std::thread::sleep(Duration::from_millis(900));
    }
    // typed one letter at a time, then Enter right after: still nothing
    for ch in ["y", "e", "s", "\r"] {
        p.send(ch).unwrap();
        std::thread::sleep(Duration::from_millis(80));
    }
    settle();
    assert_eq!(m.requests().len(), 1, "still waiting");
    assert!(!marker.exists());
    assert!(!cfg_text(&home).contains("run:"));
    std::thread::sleep(Duration::from_millis(800));
    p.send("\x1b").unwrap();
    p.expect("all-done").unwrap();
    p.expect("E> ").unwrap();
    assert!(!marker.exists());
    assert!(!cfg_text(&home).contains("run:"), "{}", cfg_text(&home));
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn multiline_asks_even_in_auto_and_a_huge_command_is_refused() {
    let home = TempDir::new().unwrap();
    let long = format!("echo {}", "x".repeat(1200));
    let m = stepped(
        "chat",
        vec![
            ("", run_call("c1", "echo a\necho b")),
            ("", run_call("c2", &long)),
            ("fin-all\n", vec![]),
        ],
    );
    let mut p = with_run(&home, &m, "run: auto\n");
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    p.expect("▶ ").unwrap();
    p.expect("⏎").unwrap();
    p.expect("Enter run").unwrap();
    settle();
    p.send("\x1b").unwrap();
    p.expect("fin-all").unwrap();
    let reqs = m.requests();
    assert!(
        reqs[2].body.contains("too long to review"),
        "{}",
        reqs[2].body
    );
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

// ---- run: judge ----------------------------------------------------------------

#[derive(Clone, Copy)]
enum J {
    Say(&'static str),
    Err500,
    /// answers after this many ms
    Slow(u64),
}

/// Agent steps for requests that carry tools; the judge's (no tools) get `j`.
fn judge_mock(steps: Vec<(&'static str, Calls)>, j: J) -> Mock {
    let n = Arc::new(AtomicUsize::new(0));
    Mock::start(move |r, c| {
        if !r.body.contains("\"tools\"") {
            match j {
                J::Say(t) => mock::chat_stream(c, &[t], Duration::ZERO),
                J::Err500 => mock::json(c, 500, "{}"),
                J::Slow(ms) => {
                    std::thread::sleep(Duration::from_millis(ms));
                    mock::chat_stream(c, &["ALLOW"], Duration::ZERO)
                }
            }
            return;
        }
        let i = n.fetch_add(1, Ordering::SeqCst).min(steps.len() - 1);
        let (text, calls) = &steps[i];
        let calls: Vec<(&str, &str, &str)> =
            calls.iter().map(|(a, b, d)| (*a, *b, d.as_str())).collect();
        if calls.is_empty() {
            mock::chat_stream(c, &[text], Duration::ZERO)
        } else {
            mock::chat_calls_stream(c, text, &calls)
        }
    })
}

fn judge_session(home: &TempDir, m: &Mock, extra: &str) -> Session {
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        format!(
            "{}run: {{mode: judge{extra}}}\n",
            config_kind_run(&m.url(), "chat")
        ),
    )
    .unwrap();
    let mut cmd = base(home);
    cmd.env("AISH_JUDGE_TIMEOUT_MS", "800");
    spawn(cmd)
}

/// Not plain (it expands a variable) and not on the hard list.
const JCMD: &str = "printf 'v-%s\\n' \"${HOME:+ok}\"";

fn judge_requests(m: &Mock) -> Vec<String> {
    m.requests()
        .iter()
        .filter(|r| !r.body.contains("\"tools\"") && r.path.contains("chat"))
        .map(|r| r.body.clone())
        .collect()
}

#[test]
fn judge_allow_runs_without_a_prompt_and_sees_only_the_basics() {
    let home = TempDir::new().unwrap();
    let m = judge_mock(run_steps(&[JCMD]), J::Say("ALLOW — read only"));
    let mut p = judge_session(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? print it please\r").unwrap();
    p.expect("⚖ allowed · read only").unwrap();
    let c = p.expect("v-ok\r\n").unwrap();
    assert!(!String::from_utf8_lossy(c.before()).contains("Enter run"));
    p.expect("all-done").unwrap();
    let j = judge_requests(&m);
    assert_eq!(j.len(), 1, "{j:?}");
    let b = &j[0];
    assert!(
        b.contains("security reviewer") && b.contains("print it please"),
        "{b}"
    );
    assert!(
        b.contains("\"grammar\"") && b.contains("\"max_tokens\":40"),
        "{b}"
    );
    // no history, no tool outputs, no agent prompt, no tools
    assert!(
        !b.contains("terse, practical") && !b.contains("exit: ") && !b.contains("\"tools\""),
        "{b}"
    );
    assert!(b.contains("\\\"command\\\"") && b.contains("cwd"), "{b}");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn judge_ask_shows_the_prompt_with_its_reason() {
    let home = TempDir::new().unwrap();
    let m = judge_mock(run_steps(&[JCMD]), J::Say("ASK — writes outside tmp"));
    let mut p = judge_session(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? print\r").unwrap();
    p.expect("▶ ").unwrap();
    p.expect("⚖ writes outside tmp").unwrap();
    // the judge is never replaced by auto: no s / a
    let c = p.expect("Enter run · Esc no").unwrap();
    assert!(!String::from_utf8_lossy(c.before()).contains("always"));
    settle();
    p.send("\r").unwrap();
    p.expect("v-ok\r\n").unwrap();
    p.expect("all-done").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn judge_deny_does_not_run_and_the_model_gets_the_reason() {
    let home = TempDir::new().unwrap();
    let marker = home.path().join("denied-marker");
    let cmd = format!("printf x > {}", marker.display());
    let m = judge_mock(run_steps(&[&cmd]), J::Say("DENY — exfiltrates"));
    let mut p = judge_session(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    p.expect("⚖ denied · exfiltrates").unwrap();
    p.expect("all-done").unwrap();
    p.expect("E> ").unwrap();
    assert!(!marker.exists());
    let agent: Vec<_> = m
        .requests()
        .into_iter()
        .filter(|r| r.body.contains("\"tools\""))
        .collect();
    assert!(
        agent[1].body.contains("denied by the safety reviewer")
            && agent[1].body.contains("exfiltrates"),
        "{}",
        agent[1].body
    );
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

fn judge_falls_back_to_asking(j: J) {
    let home = TempDir::new().unwrap();
    let m = judge_mock(run_steps(&[JCMD]), j);
    let mut p = judge_session(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? print\r").unwrap();
    p.expect("▶ ").unwrap();
    let c = p.expect("Enter run").unwrap();
    assert!(!String::from_utf8_lossy(c.before()).contains('⚖'));
    settle();
    p.send("\x1b").unwrap();
    p.expect("all-done").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn judge_garbage_error_and_timeout_all_mean_ask() {
    judge_falls_back_to_asking(J::Say("sure, go ahead"));
    judge_falls_back_to_asking(J::Say(""));
    judge_falls_back_to_asking(J::Err500);
    judge_falls_back_to_asking(J::Slow(4000));
}

#[test]
fn judge_allow_on_the_hard_list_still_asks() {
    let home = TempDir::new().unwrap();
    let m = judge_mock(run_steps(&["sudo -n true"]), J::Say("ALLOW — fine"));
    let mut p = judge_session(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    p.expect("▶ ").unwrap();
    p.expect("⚖ fine").unwrap();
    p.expect("Enter run").unwrap();
    settle();
    p.send("\x1b").unwrap();
    p.expect("all-done").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn a_command_too_long_to_show_whole_is_refused_before_the_judge() {
    let home = TempDir::new().unwrap();
    let cmd = format!("printf 'a-%s\\n' 1\n# {}", "x".repeat(3000));
    let m = judge_mock(run_steps(&[&cmd]), J::Say("ALLOW"));
    let mut p = judge_session(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? do it\r").unwrap();
    p.expect("all-done").unwrap();
    assert!(judge_requests(&m).is_empty());
    let agent: Vec<_> = m
        .requests()
        .into_iter()
        .filter(|r| r.body.contains("\"tools\""))
        .collect();
    assert!(agent[1].body.contains("too long to review"));
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn judge_prompt_append_and_replace_come_from_the_config() {
    for (extra, want, gone) in [
        (
            ", judge_prompt: {append: 'ZZ-EXTRA-RULE'}",
            "ZZ-EXTRA-RULE",
            false,
        ),
        (
            ", judge_prompt: {replace: 'ONLY-THIS-PROMPT'}",
            "ONLY-THIS-PROMPT",
            true,
        ),
    ] {
        let home = TempDir::new().unwrap();
        let m = judge_mock(run_steps(&[JCMD]), J::Say("DENY"));
        let mut p = judge_session(&home, &m, extra);
        p.expect("E> ").unwrap();
        settle();
        p.send("? x\r").unwrap();
        p.expect("all-done").unwrap();
        let j = judge_requests(&m);
        assert!(j[0].contains(want), "{}", j[0]);
        assert_eq!(!j[0].contains("security reviewer"), gone, "{}", j[0]);
        p.send_line("exit").unwrap();
        p.expect(Eof).unwrap();
    }
}

#[test]
fn ai_run_judge_is_set_shown_and_saved_into_a_mapping() {
    let home = TempDir::new().unwrap();
    let m = judge_mock(vec![("ok\n", vec![])], J::Say("ASK"));
    let mut p = judge_session(&home, &m, "");
    p.expect("E> ").unwrap();
    settle();
    p.send_line("ai run").unwrap();
    p.expect("run: judge").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send_line("ai run auto --save").unwrap();
    p.expect("run: auto · saved").unwrap();
    assert!(
        cfg_text(&home).contains("mode: auto"),
        "{}",
        cfg_text(&home)
    );
    p.expect("E> ").unwrap();
    settle();
    p.send_line("ai run judge").unwrap();
    p.expect("run: judge · this session").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}
