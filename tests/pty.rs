//! Drives the aish binary under a real pty.

use std::process::Command;
use std::time::Duration;

use expectrl::{Eof, Session};
use tempfile::TempDir;

fn session(home: &TempDir) -> Session {
    std::fs::write(home.path().join(".bashrc"), "PS1='PROMPT> '\n").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aish"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.path())
        .env("TERM", "xterm")
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "aish=debug")
        .env("XDG_STATE_HOME", home.path().join("state"));
    let mut p = Session::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(Duration::from_secs(10)));
    p
}

#[test]
fn status_line_blocks_and_exit_code() {
    let home = TempDir::new().unwrap();
    let mut p = session(&home);
    p.expect("PROMPT> ").unwrap();
    p.send_line("echo hi; false").unwrap();
    // Status line comes after the output and before the next prompt.
    p.expect("✗ 1\r\n").unwrap();
    p.expect("PROMPT> ").unwrap();
    // Fast success: no status line at all, next thing is the prompt.
    p.send_line("echo ok; true").unwrap();
    let m = p.expect("PROMPT> ").unwrap();
    let before = String::from_utf8_lossy(m.before()).into_owned();
    assert!(
        before.contains("ok") && !before.contains('✗') && !before.contains('✓'),
        "{before:?}"
    );
    // Private OSC must not reach the terminal.
    assert!(!before.contains("1337"), "{before:?}");
    p.send_line("exit 3").unwrap();
    p.expect(Eof).unwrap();
    let status = p.get_process().wait().unwrap();
    assert!(
        matches!(status, expectrl::WaitStatus::Exited(_, 3)),
        "{status:?}"
    );

    // Blocks were recorded (debug log of finished blocks).
    let log = std::fs::read_to_string(home.path().join("state/aish/aish.log")).unwrap();
    assert!(
        log.contains("cmd=echo hi; false") && log.contains("exit=Some(1)"),
        "{log}"
    );
    assert!(
        log.contains("cmd=echo ok; true") && log.contains("exit=Some(0)"),
        "{log}"
    );
}

#[test]
fn array_prompt_command_and_user_ps1_preserved() {
    let home = TempDir::new().unwrap();
    std::fs::write(
        home.path().join(".bashrc"),
        "PS1='U> '\nPROMPT_COMMAND=('echo pc1' 'echo pc2')\n",
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aish"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.path())
        .env("TERM", "xterm")
        .env("XDG_STATE_HOME", home.path().join("state"));
    let mut p = Session::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(Duration::from_secs(10)));
    p.expect("pc2").unwrap();
    p.expect("U> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

fn session_with_rc(home: &TempDir, bashrc: &str) -> Session {
    std::fs::write(home.path().join(".bashrc"), bashrc).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aish"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.path())
        .env("TERM", "xterm")
        .env("RUST_LOG", "aish=debug")
        .env("XDG_STATE_HOME", home.path().join("state"));
    let mut p = Session::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(Duration::from_secs(10)));
    p
}

#[test]
fn set_u_in_bashrc() {
    let home = TempDir::new().unwrap();
    let mut p = session_with_rc(&home, "set -u\nPS1='P> '\n");
    p.expect("P> ").unwrap();
    p.send_line("false").unwrap();
    let m = p.expect("P> ").unwrap();
    let out = String::from_utf8_lossy(m.before()).into_owned();
    assert!(out.contains('✗') && !out.contains("unbound"), "{out:?}");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
    let log = std::fs::read_to_string(home.path().join("state/aish/aish.log")).unwrap();
    assert!(
        log.contains("cmd=false") && log.contains("exit=Some(1)"),
        "{log}"
    );
}

#[test]
fn no_leak_into_children() {
    let home = TempDir::new().unwrap();
    let mut p = session_with_rc(&home, "PS1='P> '\nexport PROMPT_COMMAND='true'\n");
    p.expect("P> ").unwrap();
    p.send_line("env | grep -cE '^(PROMPT_COMMAND|PS0|AISH)=' ; bash -ic 'echo nested-ok' 2>&1")
        .unwrap();
    let m = p.expect("nested-ok").unwrap();
    let out = String::from_utf8_lossy(m.before()).into_owned();
    assert!(!out.contains("not found"), "{out:?}");
    p.expect("P> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn args_and_non_tty_exec_plain_bash() {
    let out = Command::new(env!("CARGO_BIN_EXE_aish"))
        .args(["-c", "echo X; exit 5"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"X\n");
    assert_eq!(out.status.code(), Some(5));
    let out = Command::new(env!("CARGO_BIN_EXE_aish"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
}

// ---- line editor (prompt mode) ----

/// Colors on (ghost text needs them), seeded history, no DSR answers.
fn editor_session(home: &TempDir, history: &str) -> Session {
    editor_session_rc(home, history, "PS1='E> '\n")
}

fn editor_session_rc(home: &TempDir, history: &str, bashrc: &str) -> Session {
    std::fs::write(home.path().join(".bashrc"), bashrc).unwrap();
    std::fs::write(home.path().join("hist"), history).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aish"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.path())
        .env("HISTFILE", home.path().join("hist"))
        .env("TERM", "xterm")
        .env("XDG_STATE_HOME", home.path().join("state"));
    let mut p = Session::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(Duration::from_secs(10)));
    p
}

/// Wait until the editor has taken over the prompt (quiet timer + history load).
fn settle() {
    std::thread::sleep(Duration::from_millis(400));
}

#[test]
fn ghost_text_accept_and_run() {
    let home = TempDir::new().unwrap();
    let mut p = editor_session(&home, "echo hi\n");
    p.expect("E> ").unwrap();
    settle();
    p.send("ech").unwrap();
    p.expect("o hi").unwrap(); // ghost, drawn by us, bash has seen nothing
    p.send("\x1b[C").unwrap(); // right arrow accepts
    p.send("\r").unwrap();
    p.expect("\x07hi\r\n").unwrap(); // output of the command (after OSC 133;C)
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn ctrl_c_clears_line() {
    let home = TempDir::new().unwrap();
    let mut p = editor_session(&home, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("zzzbad").unwrap();
    p.send("\x03").unwrap();
    p.expect("E> ").unwrap();
    settle();
    p.send("echo fine\r").unwrap();
    let m = p.expect("fine\r\n").unwrap();
    let before = String::from_utf8_lossy(m.before()).into_owned();
    assert!(!before.contains("not found"), "{before:?}");
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn model_line_does_not_reach_bash() {
    let home = TempDir::new().unwrap();
    let mut p = editor_session(&home, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("? hello\r").unwrap();
    p.expect("модель не настроена · ").unwrap();
    p.expect(expectrl::Regex("(ai|aish) config")).unwrap();
    p.expect("E> ").unwrap();
    settle();
    // A glob stays a shell command.
    p.send("echo ?x\r").unwrap();
    let m = p.expect("E> ").unwrap();
    let out = String::from_utf8_lossy(m.before()).into_owned();
    assert!(out.contains("?x") && !out.contains("not found"), "{out:?}");
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn editing_keys_and_submit_to_bash() {
    let home = TempDir::new().unwrap();
    let mut p = editor_session(&home, "");
    p.expect("E> ").unwrap();
    settle();
    // type, move, edit: "echo abXd" -> Ctrl-A, Alt-D kills "echo", retype
    p.send("echo abcd").unwrap();
    p.send("\x1b[D\x1b[D\x7f").unwrap(); // left left backspace -> "echo acd"
    p.send("X").unwrap(); // "echo aXcd"
    p.send("\r").unwrap();
    p.expect("aXcd\r\n").unwrap();
    p.expect("E> ").unwrap();
    // Ctrl-D on empty line reaches bash and exits it.
    settle();
    p.send("\x04").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn tab_completes_path() {
    let home = TempDir::new().unwrap();
    let mut p = editor_session(&home, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("ls /us").unwrap();
    p.send("\t").unwrap();
    p.expect("/usr/").unwrap();
    p.send("\r").unwrap(); // runs `ls /usr/`
    p.expect("bin").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn type_ahead_during_command_runs_natively_once() {
    let home = TempDir::new().unwrap();
    let mut p = editor_session(&home, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("sleep 0.6\r").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    p.send("echo ahead-ok\r").unwrap(); // typed while sleep runs, echoed by readline later
    p.expect("\x07ahead-ok\r\n").unwrap();
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn completion_function_reading_stdin_does_not_desync_sidecar() {
    let home = TempDir::new().unwrap();
    let mut p = editor_session_rc(
        &home,
        "",
        "PS1='E> '\n_f() { read -r junk; COMPREPLY=(alpha beta); }\ncomplete -F _f foo\n",
    );
    p.expect("E> ").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    for _ in 0..2 {
        p.send("foo \t").unwrap();
        p.expect("alpha").unwrap();
        p.send("\x1b").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        p.send("\x15").unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}

#[test]
fn ctrl_q_at_the_prompt_toggles_the_question_marker() {
    let home = TempDir::new().unwrap();
    let mut p = editor_session(&home, "");
    p.expect("E> ").unwrap();
    settle();
    p.send("echo zz").unwrap();
    p.send("\x11").unwrap();
    // the (accent-colored) marker is in front of the line now
    p.expect("\x1b[0;36m?\x1b[0;m echo zz").unwrap();
    p.send("\x11").unwrap(); // and gone again
    settle();
    p.send("\r").unwrap();
    p.expect("\x07zz\r\n").unwrap(); // the line ran as a plain command
    p.expect("E> ").unwrap();
    p.send_line("exit").unwrap();
    p.expect(Eof).unwrap();
}
