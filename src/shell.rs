//! Generated bash rc (shell integration) and logging setup.

use std::path::PathBuf;

use anyhow::{Context, Result};

const RC: &str = include_str!("rc.bash");

pub fn state_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("llmsh"))
}

/// Write the rc file (atomically, only if changed) and return its path.
pub fn install_rc() -> Result<PathBuf> {
    let dir = state_dir().context("no HOME")?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("rc.bash");
    if std::fs::read_to_string(&path).is_ok_and(|s| s == RC) {
        return Ok(path);
    }
    let tmp = dir.join(format!("rc.bash.{}", std::process::id()));
    std::fs::write(&tmp, RC)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Log to a file only; never to stdout/stderr. Keep the guard alive.
pub fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let (w, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(dir, "llmsh.log"));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(w)
        .with_ansi(false)
        .with_env_filter(filter)
        .init();
    Some(guard)
}
