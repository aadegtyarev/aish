//! llmsh: transparent PTY proxy around bash with command blocks.

pub mod blocks;
pub mod config;
pub mod osc;
pub mod pty;
pub mod shell;
pub mod term;

use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

/// Run the shell under the proxy; returns its exit code.
pub fn run() -> Result<i32> {
    match config::load() {
        Ok(Some(c)) => tracing::info!(providers = c.providers.len(), "config loaded"),
        Ok(None) => tracing::info!("no config"),
        Err(e) => tracing::warn!("config: {e:#}"),
    }
    let rc = shell::install_rc()?;
    let mut cmd = Command::new("bash");
    cmd.arg("--rcfile").arg(&rc).arg("-i").env("LLMSH", "1");
    let (mut child, master) = pty::spawn(cmd, &term::window_size())?;

    let raw = term::RawGuard::enter();
    let tracker = Arc::new(Mutex::new(blocks::BlockTracker::default()));
    term::forward_resizes(&master)?;

    let stdin = pty::dup_std(&std::io::stdin())?;
    let stdout = pty::dup_std(&std::io::stdout())?;
    let (m_in, m_out) = (master.try_clone()?, master);
    let is_tty = raw.is_some();
    std::thread::spawn(move || pty::pump_input(stdin, m_in, is_tty));
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        pty::pump_output(m_out, stdout, tracker);
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
