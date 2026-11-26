//! Drives the llmsh binary under a real pty.

use std::process::Command;
use std::time::Duration;

use expectrl::{Eof, Session};
use tempfile::TempDir;

fn session(home: &TempDir) -> Session {
    std::fs::write(home.path().join(".bashrc"), "PS1='PROMPT> '\n").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_llmsh"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home.path())
        .env("TERM", "xterm")
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "llmsh=debug")
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
    let log = std::fs::read_to_string(home.path().join("state/llmsh/llmsh.log")).unwrap();
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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_llmsh"));
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
