//! LLM features under a real pty against an in-process mock server.

use std::process::Command;
use std::time::{Duration, Instant};

use expectrl::{Eof, Session};
use llmsh_llm::mock::{self, Mock};
use tempfile::TempDir;

fn config(url: &str) -> String {
    format!(
        "providers:\n  - id: mock\n    url: {url}\n    type: chat\n    models: {{m: {{id: test}}}}\nroles:\n  chat: {{id: mock/m}}\n  complete: {{id: mock/m}}\n"
    )
}

fn base(home: &TempDir) -> Command {
    std::fs::write(home.path().join(".bashrc"), "PS1='E> '\n").unwrap();
    std::fs::write(home.path().join("hist"), "").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_llmsh"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.path())
        .env("HISTFILE", home.path().join("hist"))
        .env("TERM", "xterm")
        .env("XDG_STATE_HOME", home.path().join("state"));
    cmd
}

fn spawn(cmd: Command) -> Session {
    let mut p = Session::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(Duration::from_secs(10)));
    p
}

fn with_config(home: &TempDir, mock: &Mock) -> Session {
    let dir = home.path().join(".config/llmsh");
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
    p.expect("401 от mock").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn llmsh_status_switch_and_config_prefill() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("llmsh\r").unwrap();
    p.expect("chat    mock/m · test · ok ").unwrap();
    p.expect("complete").unwrap();
    p.expect("подсказки: вкл").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("llmsh off\r").unwrap();
    p.expect("подсказки: выкл").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("llmsh chat mock/m\r").unwrap();
    p.expect("chat → mock/m").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("llmsh config\r").unwrap();
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
    cmd.env("LLMSH_PROBE_OLLAMA", m.url())
        .env("LLMSH_PROBE_LMSTUDIO", "http://127.0.0.1:1/v1");
    let mut p = spawn(cmd);
    p.expect(expectrl::Regex(
        "llmsh · chat: ollama/qwen3:32b · complete: ollama/qwen2.5-coder:1.5b · (ai|llmsh) config — изменить",
    ))
    .unwrap();
    p.expect("E> ").unwrap();
    settle();
    let cfg = std::fs::read_to_string(home.path().join(".config/llmsh/config.yaml")).unwrap();
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
    cmd.env("LLMSH_PROBE_OLLAMA", "http://127.0.0.1:1/v1")
        .env("LLMSH_PROBE_LMSTUDIO", "http://127.0.0.1:1/v1");
    let mut p = spawn(cmd);
    p.expect("E> ").unwrap();
    settle();
    assert!(!home.path().join(".config/llmsh/config.yaml").exists());
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn cli_subcommands_and_bash_passthrough() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let dir = home.path().join(".config/llmsh");
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
        s.contains("chat    mock/m · test · ok") && s.contains("подсказки"),
        "{s}"
    );
    let o = run(&["config"]);
    assert!(String::from_utf8_lossy(&o.stdout)
        .trim()
        .ends_with("llmsh/config.yaml"));
    assert!(String::from_utf8_lossy(&run(&["--version"]).stdout).starts_with("llmsh "));
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
fn llmsh_off_silences_suggestions() {
    let home = TempDir::new().unwrap();
    let m = suggest_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    settle();
    p.send("llmsh off\r").unwrap();
    p.expect("выкл").unwrap();
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
    cmd.env("LLMSH_PROBE_OLLAMA", "http://127.0.0.1:1/v1")
        .env("LLMSH_PROBE_LMSTUDIO", "http://127.0.0.1:1/v1")
        .env("LLMSH_PROBE_LLAMACPP", m.url());
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
    p.send("ai\r").unwrap();
    p.expect("chat    mock/m · test · ok ").unwrap();
    p.expect("подсказки: вкл").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("ai off\r").unwrap();
    p.expect("подсказки: выкл").unwrap();
    p.expect("E> ").unwrap();
    settle();
    // the long form always works
    p.send("llmsh on\r").unwrap();
    p.expect("подсказки: вкл").unwrap();
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
    let dir = home.path().join(".config/llmsh");
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
    let dir = home.path().join(".config/llmsh");
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
    p.send("ai\r").unwrap();
    p.expect("chat    mock/m").unwrap();
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
    p.send("mm\r").unwrap();
    p.expect("chat    mock/m · test · ok ").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("ai\r").unwrap();
    p.expect("ai: command not found").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn command_off_leaves_ai_to_bash_but_llmsh_works() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let mut p = with_command(&home, &m, "off");
    p.expect("E> ").unwrap();
    settle();
    p.send("ai\r").unwrap();
    p.expect("ai: command not found").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("llmsh\r").unwrap();
    p.expect("chat    mock/m · test · ok ").unwrap();
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
    config(url)
        .replace("type: chat", &format!("type: {kind}"))
        .replace("  complete: {id: mock/m}\n", "")
}

fn with_kind(home: &TempDir, m: &Mock, kind: &str) -> Session {
    let dir = home.path().join(".config/llmsh");
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
    // read-only command ran by llmsh itself: dim line, no output shown
    p.expect("$ ls Cargo.toml · 1 строк").unwrap();
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
    p.expect("лимит шагов (12)").unwrap();
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
    let dir = home.path().join(".config/llmsh");
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
    p.expect("статус моделей").unwrap(); // transient description
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
