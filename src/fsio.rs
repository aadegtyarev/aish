//! Atomic file writes: a temp file next to the target, then a rename.

use std::io::{ErrorKind, Result, Write};
use std::os::unix::fs::{fchown, MetadataExt, PermissionsExt};
use std::path::Path;

use tempfile::NamedTempFile;

fn temp(path: &Path, content: &str, mode: u32) -> Result<NamedTempFile> {
    let mut f = NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    f.write_all(content.as_bytes())?;
    f.as_file()
        .set_permissions(std::fs::Permissions::from_mode(mode))?;
    f.as_file().sync_all()?;
    Ok(f)
}

/// Replace `path` atomically. A symlink is followed (the target changes, the
/// link stays). Mode: `mode`, else the file's own, else 0600; owner is kept.
pub fn write_atomic(path: &Path, content: &str, mode: Option<u32>) -> Result<()> {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let meta = std::fs::metadata(&path).ok();
    let own = meta.as_ref().map(|m| m.mode() & 0o7777);
    let f = temp(&path, content, mode.or(own).unwrap_or(0o600))?;
    if let Some(m) = &meta {
        let _ = fchown(f.as_file(), Some(m.uid()), Some(m.gid()));
    }
    f.persist(&path).map_err(|e| e.error)?;
    if let Some(d) = path.parent() {
        std::fs::File::open(d)?.sync_all()?;
    }
    Ok(())
}

/// Create `path` (0600) unless it exists: never overwrites, even in a race.
/// Returns whether it was written.
pub fn write_new(path: &Path, content: &str) -> Result<bool> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    match temp(path, content, 0o600)?.persist_noclobber(path) {
        Ok(_) => Ok(true),
        Err(e) if e.error.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_mode_wins_and_new_files_are_private() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.yaml");
        std::fs::write(&p, "a: 1\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(&p, "b: 2\n", Some(0o600)).unwrap();
        let q = d.path().join("new");
        write_atomic(&q, "x", None).unwrap();
        for f in [&p, &q] {
            assert_eq!(std::fs::metadata(f).unwrap().mode() & 0o777, 0o600);
        }
    }
}
