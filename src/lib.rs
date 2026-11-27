//! aish: transparent PTY proxy around bash with command blocks.

pub mod blocks;
pub mod complete;
pub mod config;
pub mod editor;
pub mod init;
pub mod llm;
pub mod module;
pub mod modules;
pub mod osc;
pub mod pty;
pub mod shell;
pub mod term;
pub mod update;

use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

/// True for `-aish` style argv[0] (login shell).
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
    if interactive {
        // The fallback after a failed start: ~/.bashrc must not start aish again.
        cmd.env("AISH_DISABLE", "1");
    }
    cmd.args(args).exec()
}

/// Run the shell under the proxy; returns its exit code.
pub fn run() -> Result<i32> {
    if std::env::var_os("AISH_TEST_FAIL_START").is_some() {
        anyhow::bail!("start failure requested by AISH_TEST_FAIL_START");
    }
    let mut enabled_modules: Option<Vec<String>> = None;
    let mut ask_key = Some(editor::overlay::AskKey::DEFAULT);
    match config::load() {
        Ok(Some(c)) => {
            tracing::info!(providers = c.providers.len(), "config loaded");
            ask_key = editor::overlay::AskKey::from_config(&c.keys.ask);
            enabled_modules = c.modules;
        }
        Ok(None) => tracing::info!("no config"),
        Err(e) => tracing::warn!("config: {e:#}"),
    }
    let llm = llm::Llm::new();
    llm::firstrun::spawn(llm.clone());
    update::spawn_check(llm.clone());
    pty::clear_nonblock(0);
    pty::clear_nonblock(1);
    let rc = shell::install_rc()?;
    let signals = term::Signals::register()?;
    let mut cmd = Command::new("bash");
    // Session nonce: our OSC 133 marks carry it, foreign ones (ssh, nested
    // shells with their own integration) are ignored.
    let nonce = format!(
        "{:x}{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    cmd.arg("--rcfile")
        .arg(&rc)
        .arg("-i")
        .env("AISH", "1")
        .env("AISH_NONCE", &nonce);
    if is_login() {
        cmd.env("AISH_LOGIN", "1");
    }
    let (mut child, master) = pty::spawn(cmd, &term::window_size())?;

    let raw = term::RawGuard::enter();
    let tracker = Arc::new(Mutex::new(blocks::BlockTracker::with_nonce(nonce.clone())));
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
    let (ed, wake_r) = if is_tty && editor::ctl::wanted() {
        let (r, w) = nix::unistd::pipe()?;
        for fd in [&r, &w] {
            nix::fcntl::fcntl(
                fd.as_raw_fd(),
                nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
            )?;
        }
        let ctl = editor::ctl::Controller::start(
            stdout.try_clone()?,
            tracker.clone(),
            enabled_modules.as_deref(),
            nonce,
            std::fs::File::from(w),
            llm,
            ask_key,
        );
        (Some(ctl), Some(std::fs::File::from(r)))
    } else {
        (None, None)
    };
    let gate = ed.as_ref().map(|c| editor::ctl::lock(c).gate());
    signals.watch(m_sig, child.id(), ed.clone(), gate.clone());
    let ed_in = ed.clone().zip(wake_r);
    std::thread::spawn(move || pty::pump_input(stdin, m_in, is_tty, ed_in));
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        pty::pump_output(master, stdout, tracker, ed);
        let _ = done_tx.send(());
    });

    let status = child.wait()?;
    if let Some(g) = &gate {
        g.child_gone(); // an open overlay closes and lets the held output through
    }
    // Drain remaining output, but don't hang if a background job holds the pty.
    let _ = done_rx.recv_timeout(Duration::from_millis(300));
    if let Some(g) = gate {
        g.emergency_restore(); // the shell ended with the overlay open
    }
    drop(raw);
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}
