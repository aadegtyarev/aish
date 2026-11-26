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

/// The sidecar that reports the shell's aliases/functions needs a moment.
fn sidecar_ready() {
    std::thread::sleep(Duration::from_millis(2200));
}

#[test]
fn ai_is_the_short_command_when_the_shell_has_no_ai() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let mut p = with_config(&home, &m);
    p.expect("E> ").unwrap();
    sidecar_ready();
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

#[test]
fn users_own_ai_wins_over_the_builtin() {
    let home = TempDir::new().unwrap();
    let m = answering_mock();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let ai = bin.join("ai");
    std::fs::write(&ai, "#!/bin/sh\necho real-ai-ran\n").unwrap();
    std::os::unix::fs::PermissionsExt::set_mode(
        &mut std::fs::metadata(&ai).unwrap().permissions(),
        0o755,
    );
    std::fs::set_permissions(&ai, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let dir = home.path().join(".config/llmsh");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), config(&m.url())).unwrap();
    std::fs::write(
        home.path().join(".bashrc"),
        format!("PS1='E> '\nexport PATH={}:$PATH\n", bin.display()),
    )
    .unwrap();
    std::fs::write(home.path().join("hist"), "").unwrap();
    let mut cmd = base(&home);
    // base() rewrites .bashrc: restore ours afterwards
    std::fs::write(
        home.path().join(".bashrc"),
        format!("PS1='E> '\nexport PATH={}:$PATH\n", bin.display()),
    )
    .unwrap();
    cmd.env(
        "PATH",
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
    );
    let mut p = spawn(cmd);
    p.expect("E> ").unwrap();
    sidecar_ready();
    p.send("ai\r").unwrap();
    p.expect("real-ai-ran").unwrap();
    p.expect("E> ").unwrap();
    settle();
    // llmsh still works; `?` points at the long form
    p.send("llmsh\r").unwrap();
    p.expect("chat    mock/m").unwrap();
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
