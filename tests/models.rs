//! `ai help | models | model | profile`, fallback between models, typos in our
//! own commands: under a pty, at 80 and 36 columns, against mock servers.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::Duration;

use aish_llm::mock::{self, Mock};
use common::{spawn_sized, Term};
use tempfile::TempDir;

const LONG_ID: &str = "deepseek-chat-very-long-model-identifier-2026";

fn config(url: &str) -> String {
    format!(
        "providers:\n  - id: local\n    url: http://127.0.0.1:9/v1\n    type: chat\n    models: {{mellum: {{id: mellum-1}}, oss: {{id: oss}}}}\n  - id: cloud\n    url: {url}\n    type: chat\n    models: {{ds: {{id: {LONG_ID}}}}}\n# my comment\nroles:\n  chat: {{id: [mellum, ds]}}   # fallback\n  complete: {{id: ds}}\n"
    )
}

fn mock() -> Mock {
    Mock::start(|r, c| {
        if r.path.ends_with("/models") {
            mock::json(c, 200, r#"{"data":[{"id":"x"}]}"#);
        } else {
            mock::chat_stream(c, &["ANSWER-OK\n"], Duration::from_millis(10));
        }
    })
}

fn start(home: &TempDir, m: &Mock, cols: u16) -> Term {
    let dir = home.path().join(".config/aish");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("config.yaml");
    std::fs::write(&cfg, config(&m.url())).unwrap();
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(home.path().join(".bashrc"), "PS1='E> '\n").unwrap();
    std::fs::write(home.path().join("hist"), "").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aish"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.path())
        .env("HISTFILE", home.path().join("hist"))
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env("INPUTRC", "/dev/null")
        .env("XDG_STATE_HOME", home.path().join("state"))
        .env("AISH_TEST_UPDATE_URL", "http://127.0.0.1:9/");
    let st = home.path().join("state/aish");
    std::fs::create_dir_all(&st).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(st.join("update-check"), format!("{now}\n\n{now}\n")).unwrap();
    let mut t = Term::new(spawn_sized(cmd, cols, 40), cols, 40);
    t.wait_for("E>");
    t.pump_for(Duration::from_millis(500));
    t
}

/// Every output row of the command keeps the bar, fits and is not wrapped.
fn rows_fit(t: &Term) {
    let c = t.scr.cols;
    for y in 0..t.scr.rows {
        let row = t.scr.row_text(y);
        assert!(
            row.chars().count() < c,
            "row {y} too long\n{}",
            t.scr.dump()
        );
        assert!(!t.scr.row_wrapped(y), "row {y} wrapped\n{}", t.scr.dump());
        if row.starts_with('│') {
            assert!(row.starts_with("│ "), "{row:?}");
        }
    }
}

fn run(t: &mut Term, line: &str, wait: &str) {
    t.send(line);
    t.send("\r");
    t.wait_for(wait);
    t.pump_for(Duration::from_millis(300));
}

#[test]
fn help_and_models_fit() {
    for cols in [80u16, 36] {
        let home = TempDir::new().unwrap();
        let m = mock();
        let mut t = start(&home, &m, cols);
        run(&mut t, "ai help", "эта справка");
        rows_fit(&t);
        let all = t.scr.everything();
        for w in ["status", "models", "profile", "config"] {
            assert!(all.contains(w), "{w}\n{}", t.scr.dump());
        }
        run(&mut t, "ai models", "cloud");
        t.pump_for(Duration::from_millis(300));
        rows_fit(&t);
        let all = t.scr.everything();
        assert!(
            all.contains("local") && all.contains("mellum"),
            "{}",
            t.scr.dump()
        );
        assert!(
            all.contains("chat") && all.contains("complete"),
            "{}",
            t.scr.dump()
        );
        if cols == 36 {
            assert!(all.contains('…'), "id not cut\n{}", t.scr.dump());
            assert!(!all.contains(LONG_ID));
        } else {
            assert!(all.contains(LONG_ID));
        }
        run(&mut t, "ai -h", "эта справка");
    }
}

#[test]
fn fallback_to_the_next_model_with_a_dim_line() {
    for cols in [80u16, 36] {
        let home = TempDir::new().unwrap();
        let m = mock();
        let mut t = start(&home, &m, cols);
        run(&mut t, "? hi", "ANSWER-OK");
        let all = t.scr.everything();
        assert!(all.contains("↪ ds · mellum недоступна"), "{}", t.scr.dump());
        let y = t.scr.find_row("↪ ds").unwrap();
        assert!(t.scr.is_dim(2, y), "{}", t.scr.dump());
        rows_fit(&t);
        // remembered: the next question goes straight to ds
        run(&mut t, "? again", "ANSWER-OK");
        t.pump_for(Duration::from_millis(300));
        assert_eq!(
            t.scr.everything().matches("↪").count(),
            1,
            "{}",
            t.scr.dump()
        );
        let chats = m
            .requests()
            .iter()
            .filter(|r| !r.path.ends_with("/models"))
            .count();
        assert_eq!(chats, 2);
    }
}

#[test]
fn model_command_persists_and_keeps_the_rest() {
    let home = TempDir::new().unwrap();
    let m = mock();
    let mut t = start(&home, &m, 36);
    run(&mut t, "ai model chat oss", "chat →");
    rows_fit(&t);
    let cfg = home.path().join(".config/aish/config.yaml");
    let got = std::fs::read_to_string(&cfg).unwrap();
    assert_eq!(
        got,
        config(&m.url()).replace("[mellum, ds]", "[oss, mellum, ds]")
    );
    assert_eq!(
        std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // `ai chat <ref>` is the same, `ai model <ref>` = chat; ds moves up
    run(&mut t, "ai chat ds", "chat → ds");
    let got = std::fs::read_to_string(&cfg).unwrap();
    assert!(
        got.contains("chat: {id: [ds, oss, mellum]}   # fallback"),
        "{got}"
    );
    run(&mut t, "ai model complete mellum", "complete →");
    let got = std::fs::read_to_string(&cfg).unwrap();
    assert!(got.contains("complete: {id: [mellum, ds]}"), "{got}");
    assert!(got.contains("# my comment\nroles:\n"));
}

#[test]
fn profiles_switch_and_persist() {
    let home = TempDir::new().unwrap();
    let m = mock();
    let mut t = start(&home, &m, 36);
    let cfg = home.path().join(".config/aish/config.yaml");
    let mut src = config(&m.url());
    src.push_str("profiles:\n  local: {chat: [mellum], complete: oss}\n  inet: {chat: [ds, mellum], complete: ds}\n");
    std::fs::write(&cfg, &src).unwrap();
    run(&mut t, "ai profile", "inet");
    t.assert_clean();
    run(&mut t, "ai profile inet", "профиль →");
    let got = std::fs::read_to_string(&cfg).unwrap();
    assert_eq!(got, format!("{src}profile: inet\n"));
    run(&mut t, "ai model chat mellum", "chat →");
    let got = std::fs::read_to_string(&cfg).unwrap();
    assert!(
        got.contains("inet: {chat: [mellum, ds], complete: ds}"),
        "{got}"
    );
    assert!(
        got.contains("chat: {id: [mellum, ds]}   # fallback"),
        "{got}"
    );
    run(&mut t, "ai profile nope", "нет профиля");
}

#[test]
fn typos_get_a_hint_never_bash() {
    for cols in [80u16, 36] {
        let home = TempDir::new().unwrap();
        let m = mock();
        let mut t = start(&home, &m, cols);
        run(&mut t, "ai chst тест", "может, chat?");
        let chats = |m: &Mock| {
            m.requests()
                .iter()
                .filter(|r| !r.path.ends_with("/models"))
                .count()
        };
        assert_eq!(chats(&m), 0, "a typo must not ask the model");
        run(&mut t, "ai models", "cloud");
        assert_eq!(chats(&m), 0);
        // anything else: the hint and the help, no request, never bash
        run(&mut t, "ai тест", "эта справка");
        assert!(
            t.scr.everything().contains("спросить модель: ? <вопрос>"),
            "{}",
            t.scr.dump()
        );
        assert_eq!(chats(&m), 0);
        run(&mut t, "ai chat dsx", "может, ds?");
        run(&mut t, "ai chat zzzzz", "есть:");
        run(&mut t, "ai model chta ds", "может, chat?");
        rows_fit(&t);
        let all = t.scr.everything();
        assert!(
            !all.contains("не найдена") && !all.contains("not found"),
            "{}",
            t.scr.dump()
        );
    }
}

#[test]
fn our_command_is_not_highlighted_as_unknown() {
    let home = TempDir::new().unwrap();
    let m = mock();
    let mut t = start(&home, &m, 80);
    let mark = t.scr.raw.len();
    t.send("zzqx");
    t.pump_for(Duration::from_millis(500));
    let bad = String::from_utf8_lossy(&t.scr.raw[mark..]).to_string();
    assert!(
        bad.contains("31mzzqx") || bad.contains("31mz"),
        "control: {bad:?}"
    );
    t.send("\x15");
    let mark = t.scr.raw.len();
    t.send("ai ");
    t.pump_for(Duration::from_millis(500));
    let ok = String::from_utf8_lossy(&t.scr.raw[mark..]).to_string();
    let at = ok.rfind("ai").expect("ai echoed");
    assert!(
        !ok[at.saturating_sub(10)..at].contains("31m"),
        "red ai: {ok:?}"
    );
}
