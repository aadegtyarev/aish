//! Command resolution for highlighting: PATH executables, builtins, keywords,
//! and (from the completion sidecar) aliases and functions.

use std::collections::HashSet;
use std::ffi::OsString;
use std::sync::{Arc, RwLock};

const BUILTINS: &[&str] = &[
    ".",
    ":",
    "[",
    "alias",
    "bg",
    "bind",
    "break",
    "builtin",
    "caller",
    "cd",
    "command",
    "compgen",
    "complete",
    "compopt",
    "continue",
    "declare",
    "dirs",
    "disown",
    "echo",
    "enable",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "getopts",
    "hash",
    "help",
    "history",
    "jobs",
    "kill",
    "let",
    "local",
    "logout",
    "mapfile",
    "popd",
    "printf",
    "pushd",
    "pwd",
    "read",
    "readarray",
    "readonly",
    "return",
    "set",
    "shift",
    "shopt",
    "source",
    "suspend",
    "test",
    "times",
    "trap",
    "true",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "wait",
];
const KEYWORDS: &[&str] = &[
    "!", "[[", "]]", "{", "}", "case", "coproc", "do", "done", "elif", "else", "esac", "fi", "for",
    "function", "if", "in", "select", "then", "time", "until", "while",
];

#[derive(Default)]
struct Inner {
    path_cmds: HashSet<Box<str>>,
    extra: HashSet<Box<str>>,
    /// PATH value the set was built from (env, or the sidecar's).
    path_env: OsString,
    /// PATH came from the sidecar: env changes are irrelevant then.
    pinned: bool,
    ready: bool,
}

/// Shared handle; clone it into the highlighter and the updating threads.
#[derive(Clone, Default)]
pub struct Resolver(Arc<RwLock<Inner>>);

/// Executable names in the directories of `path` (env PATH if `None`).
pub fn scan_path(path: Option<OsString>) -> (OsString, HashSet<Box<str>>) {
    let path = path.unwrap_or_else(|| std::env::var_os("PATH").unwrap_or_default());
    let mut set = HashSet::new();
    for dir in std::env::split_paths(&path) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| !t.is_dir()) {
                if let Some(n) = e.file_name().to_str() {
                    set.insert(n.into());
                }
            }
        }
    }
    (path, set)
}

impl Resolver {
    pub fn set_path(&self, path: OsString, cmds: HashSet<Box<str>>, pinned: bool) {
        if let Ok(mut g) = self.0.write() {
            g.pinned = pinned;
            g.path_env = path;
            g.path_cmds = cmds;
            g.ready = true;
        }
    }

    pub fn set_extra(&self, names: impl IntoIterator<Item = String>) {
        if let Ok(mut g) = self.0.write() {
            g.extra = names.into_iter().map(String::into_boxed_str).collect();
        }
    }

    /// Rescan when our own PATH changed since the last scan.
    pub fn refresh_if_stale(&self) {
        let stale = self.0.read().is_ok_and(|g| {
            g.ready && !g.pinned && std::env::var_os("PATH").unwrap_or_default() != g.path_env
        });
        if stale {
            let (p, c) = scan_path(None);
            self.set_path(p, c, false);
        }
    }

    pub fn resolves(&self, w: &str) -> bool {
        let Ok(g) = self.0.read() else { return true };
        if !g.ready
            || BUILTINS.contains(&w)
            || KEYWORDS.contains(&w)
            || g.extra.contains(w)
            || g.path_cmds.contains(w)
        {
            return true;
        }
        // Paths are not stat'ed: that would hit the file system on every key,
        // against the wrong directory (ours, not the shell's) and possibly a
        // frozen mount. Treat them as runnable.
        w.contains('/') || w.starts_with('~')
    }
}
