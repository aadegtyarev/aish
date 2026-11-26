//! llmsh: transparent PTY proxy around bash with command blocks.

pub mod blocks;
pub mod config;
pub mod editor;
pub mod osc;
pub mod pty;
pub mod shell;
pub mod term;

use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

/// True for `-llmsh` style argv[0] (login shell).
pub fn is_login() -> bool {
    std::env::args_os()
        .next()
        .is_some_and(|a| a.to_string_lossy().starts_with('-'))
}

/// Replace this process with plain bash. Only returns on failure.
pub fn exec_bash(args: &[std::ffi::OsString], interactive: bool) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("bash");
    if is_login() {
        cmd.arg("-l");
    }
    if interactive {
        cmd.arg("-i");
    }
    cmd.args(args).exec()
}

/// Run the shell under the proxy; returns its exit code.
pub fn run() -> Result<i32> {
    match config::load() {
        Ok(Some(c)) => tracing::info!(providers = c.providers.len(), "config loaded"),
        Ok(None) => tracing::info!("no config"),
        Err(e) => tracing::warn!("config: {e:#}"),
    }
    pty::clear_nonblock(0);
    pty::clear_nonblock(1);
    let rc = shell::install_rc()?;
    let signals = term::Signals::register()?;
    let mut cmd = Command::new("bash");
    cmd.arg("--rcfile").arg(&rc).arg("-i").env("LLMSH", "1");
    if is_login() {
        cmd.env("LLMSH_LOGIN", "1");
    }
    let (mut child, master) = pty::spawn(cmd, &term::window_size())?;

    let raw = term::RawGuard::enter();
    let tracker = Arc::new(Mutex::new(blocks::BlockTracker::default()));
    let setup = (|| -> Result<_> {
        let stdin = pty::dup_std(&std::io::stdin())?;
        let stdout = pty::dup_std(&std::io::stdout())?;
        Ok((stdin, stdout, master.try_clone()?, master.try_clone()?))
    })();
    let (stdin, stdout, m_in, m_sig) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    };
    let is_tty = raw.is_some();
    let ed = if is_tty && editor::ctl::wanted() {
        Some(editor::ctl::Controller::start(
            m_in.try_clone()?,
            stdout.try_clone()?,
        ))
    } else {
        None
    };
    signals.watch(m_sig, child.id(), ed.clone());
    let ed_in = ed.clone();
    std::thread::spawn(move || pty::pump_input(stdin, m_in, is_tty, ed_in));
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        pty::pump_output(master, stdout, tracker, ed);
        let _ = done_tx.send(());
    });

    let status = child.wait()?;
    // Drain remaining output, but don't hang if a background job holds the pty.
    let _ = done_rx.recv_timeout(Duration::from_millis(300));
    drop(raw);
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}
