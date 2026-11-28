//! Generated bash rc (shell integration) and logging setup.

use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::prelude::*;

const RC: &str = include_str!("rc.bash");

pub fn state_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("aish"))
}

/// Write the rc file (atomically, only if changed) and return its path.
pub fn install_rc() -> Result<PathBuf> {
    install_file("rc.bash", RC)
}

/// Write `content` to `<state dir>/<name>` (atomically, only if changed).
pub fn install_file(name: &str, content: &str) -> Result<PathBuf> {
    let dir = state_dir().context("no HOME")?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(name);
    if std::fs::read_to_string(&path).is_ok_and(|s| s == content) {
        return Ok(path);
    }
    crate::fsio::write_atomic(&path, content, None)?;
    Ok(path)
}

/// Log to `aish.log` only; never to stdout/stderr. Level from `RUST_LOG`
/// (`info` or `aish=debug,...`), default info.
pub fn init_logging() {
    let Some(dir) = state_dir() else { return };
    let file = std::fs::create_dir_all(&dir).ok().and_then(|()| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("aish.log"))
            .ok()
    });
    let Some(file) = file else { return };
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse::<Targets>().ok())
        .unwrap_or_else(|| Targets::new().with_default(tracing::Level::INFO));
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .with_filter(filter),
        )
        .init();
}
