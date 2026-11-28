//! The read-only allowlist: which commands the agent may run by itself.
//!
//! A command line is lexed like a shell would (quotes, escapes) and turned
//! into a *plan*: a pipeline of argv vectors that aish executes itself with
//! no shell involved, so quoting is only our tokenizer's job. Every simple
//! command must be on the list and every option must be a known safe one
//! (exact names: no GNU abbreviations). Anything not understood is "not
//! read-only": the caller then hands the command to the user instead.
//! When in doubt: refuse. A false negative costs one Enter.

use super::lex::{lex, Tok};
use std::path::{Component, Path, PathBuf};

/// What to execute: stages joined by pipes, argv each.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub stages: Vec<Vec<String>>,
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn expand_tilde(p: &str) -> String {
    let h = home();
    match p.strip_prefix('~') {
        Some("") if !h.is_empty() => h,
        Some(r) if r.starts_with('/') && !h.is_empty() => format!("{h}{r}"),
        _ => p.to_owned(),
    }
}

/// Directories (under `~`) whose content must never go to a model by our own
/// hand: matched as path components, walked by no search, excluded by name.
const SECRET_DIRS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".password-store",
    ".kube",
    ".docker",
    ".config/gcloud",
    ".config/gh",
    ".config/aider",
    ".config/aish",
];

/// Files whose content must never go to a model by our own hand.
pub fn is_secret_path(p: &str) -> bool {
    let e = expand_tilde(p);
    let parts: Vec<String> = e
        .split('/')
        .filter(|x| !x.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let Some(base) = parts.last() else {
        return false;
    };
    let dirs = &parts[..parts.len() - 1];
    if SECRET_DIRS.iter().any(|d| {
        let want: Vec<&str> = d.split('/').collect();
        parts.windows(want.len()).any(|w| w == want)
    }) {
        return true;
    }
    base.starts_with(".env")
        || base.ends_with(".env")
        || base.starts_with("id_")
        || [".key", ".pem", ".p12", ".pfx", ".kdbx", ".keystore"]
            .iter()
            .any(|x| base.ends_with(x))
        || [
            ".git-credentials",
            ".npmrc",
            ".pypirc",
            ".pgpass",
            ".netrc",
            "_netrc",
            ".bash_history",
            ".zsh_history",
            ".sh_history",
            ".python_history",
            ".psql_history",
            ".mysql_history",
            ".node_repl_history",
            ".lesshst",
        ]
        .contains(&base.as_str())
        || (parts.first().map(String::as_str) == Some("etc")
            && (base.starts_with("shadow")
                || base.starts_with("gshadow")
                || base.starts_with("sudoers")
                || dirs.iter().any(|d| d.starts_with("sudoers"))))
        || (parts.first().map(String::as_str) == Some("proc")
            && ["environ", "cmdline", "mem", "maps"].contains(&base.as_str()))
        || (e.ends_with(".config/aish/config.yaml"))
}

/// `w` as the kernel resolves it from `cwd`: `~` expanded, symlinks followed
/// before `..`; a path that does not exist yet resolves its deepest existing
/// ancestor.
fn resolve(w: &str, cwd: &str) -> PathBuf {
    let joined = Path::new(cwd).join(expand_tilde(w));
    let (mut cur, mut rest) = (joined.as_path(), Vec::new());
    loop {
        if let Ok(real) = std::fs::canonicalize(cur) {
            return rest.iter().rev().fold(real, |p, c| p.join(c));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                cur = parent;
            }
            _ => return joined,
        }
    }
}

/// Another process's files (`/proc/<pid>/environ`, `/dev/fd/..` leads there).
fn in_proc_pid(p: &Path) -> bool {
    p.strip_prefix("/proc")
        .ok()
        .and_then(|r| r.components().next())
        .and_then(|c| c.as_os_str().to_str())
        .is_some_and(|c| c == "self" || c == "thread-self" || c.bytes().all(|b| b.is_ascii_digit()))
}

/// `w` passes through another process's files on the way: `/proc/<pid>/root/x`
/// and `/dev/fd/../../<pid>/cwd/x` resolve to paths outside /proc.
fn via_proc_pid(w: &str, cwd: &str) -> bool {
    Path::new(cwd)
        .join(expand_tilde(w))
        .ancestors()
        .any(|p| in_proc_pid(p) || std::fs::canonicalize(p).is_ok_and(|r| in_proc_pid(&r)))
}

/// A recursive search from `root` (resolved) would walk into secret directories.
fn root_too_wide(root: &Path) -> bool {
    let h = home();
    let mut dangerous: Vec<PathBuf> = vec![
        PathBuf::from("/etc/sudoers.d"),
        PathBuf::from("/proc"),
        PathBuf::from("/root"),
    ];
    if !h.is_empty() {
        dangerous.push(PathBuf::from(&h));
        dangerous.extend(SECRET_DIRS.iter().map(|d| Path::new(&h).join(d)));
    }
    // the root itself, or any ancestor of a dangerous directory
    in_proc_pid(root)
        || dangerous
            .iter()
            .map(|d| std::fs::canonicalize(d).unwrap_or_else(|_| d.clone()))
            .any(|d| d.starts_with(root))
}

/// Option spec of one command: exact names only.
struct Spec {
    short: &'static str,
    short_val: &'static str,
    long: &'static [&'static str],
    long_val: &'static [&'static str],
    digits: bool,
}

const NONE: Spec = Spec {
    short: "",
    short_val: "",
    long: &[],
    long_val: &[],
    digits: false,
};

#[derive(Default)]
struct Parsed {
    flags: Vec<String>,
    pos: Vec<String>,
    vals: Vec<String>,
}

impl Parsed {
    fn has(&self, f: &str) -> bool {
        self.flags.iter().any(|x| x == f)
    }
}

fn parse(spec: &Spec, args: &[String]) -> Option<Parsed> {
    let mut p = Parsed::default();
    let mut done = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        i += 1;
        if done || a == "-" || !a.starts_with('-') {
            p.pos.push(a.clone());
            continue;
        }
        if a == "--" {
            done = true;
            continue;
        }
        if let Some(l) = a.strip_prefix("--") {
            let (name, val) = match l.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (l, None),
            };
            if spec.long.contains(&name) {
                p.flags.push(name.to_owned());
                p.vals.extend(val);
            } else if spec.long_val.contains(&name) {
                p.flags.push(name.to_owned());
                match val {
                    Some(v) => p.vals.push(v),
                    None => {
                        p.vals.push(args.get(i)?.clone());
                        i += 1;
                    }
                }
            } else {
                return None;
            }
            continue;
        }
        let cl: Vec<char> = a[1..].chars().collect();
        for (k, &c) in cl.iter().enumerate() {
            if spec.digits && c.is_ascii_digit() {
                continue;
            }
            if spec.short.contains(c) {
                p.flags.push(c.to_string());
            } else if spec.short_val.contains(c) {
                p.flags.push(c.to_string());
                let rest: String = cl[k + 1..].iter().collect();
                if rest.is_empty() {
                    p.vals.push(args.get(i)?.clone());
                    i += 1;
                } else {
                    p.vals.push(rest);
                }
                break;
            } else {
                return None;
            }
        }
    }
    Some(p)
}

/// Whole read-only git commands, compared word for word: git takes some
/// option values only attached (`-U3`, `--branches=x`), so there is no option
/// parsing to get out of step with it.
const GIT_READ: &[&[&str]] = &[
    &["status"],
    &["status", "-s"],
    &["status", "--short"],
    &["status", "-sb"],
    &["diff"],
    &["diff", "--stat"],
    &["diff", "--cached"],
    &["diff", "--staged"],
    &["log", "--oneline"],
    &["branch"],
    &["branch", "-a"],
    &["remote", "-v"],
    &["rev-parse", "--show-toplevel"],
];

fn git_plan(args: &[String]) -> Option<Vec<String>> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let a = a.strip_prefix(&["--no-pager"]).unwrap_or(&a);
    let sub = *a.first()?;
    let count = |s: &str| {
        s.strip_prefix('-')
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()) && !n.is_empty())
    };
    let ok = GIT_READ.contains(&a) || matches!(a, ["log", n] | ["log", "--oneline", n] if count(n));
    // (no external diff or textconv program: the flags come first)
    let guard: &[&str] = if matches!(sub, "diff" | "log") {
        &["--no-ext-diff", "--no-textconv"]
    } else {
        &[]
    };
    ok.then(|| {
        ["git", "--no-pager", sub]
            .into_iter()
            .chain(guard.iter().copied())
            .chain(a[1..].iter().copied())
            .map(str::to_owned)
            .collect()
    })
}

fn ps_ok(args: &[String]) -> bool {
    // BSD-style first word (`aux`): letters only, never `e` (environment)
    let mut rest = args;
    if let Some(f) = args.first() {
        if !f.starts_with('-') && !f.chars().all(|c| c.is_ascii_digit() || c == ',') {
            if !f.chars().all(|c| "aAxuUwlfjmrhTvV".contains(c)) {
                return false;
            }
            rest = &args[1..];
        }
    }
    let Some(p) = parse(&PS, rest) else {
        return false;
    };
    let fmt_bad = |s: &str| {
        let l = s.to_ascii_lowercase();
        l.contains("env")
    };
    !p.vals.iter().any(|v| fmt_bad(v))
        && p.pos
            .iter()
            .all(|x| x.chars().all(|c| c.is_ascii_digit() || c == ','))
}

/// Parsed stage: argv to run, directories a recursive command would walk.
struct Stage {
    argv: Vec<String>,
    roots: Option<Vec<String>>,
}

/// Names a recursive search must never read, wherever they are in the tree.
const SECRET_NAMES: &[&str] = &[
    ".env*",
    "*.env",
    "id_*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.kdbx",
    "*.keystore",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".pgpass",
    ".netrc",
    "_netrc",
    ".bash_history",
    ".zsh_history",
    ".sh_history",
    ".python_history",
    ".psql_history",
    ".mysql_history",
    ".node_repl_history",
    ".lesshst",
    "shadow*",
    "gshadow*",
    "sudoers*",
    "hosts.yml",
];

/// `argv` with exclusions for every secret pattern, placed last (before a
/// `--`) so they win over any `--include`/`-g` the user gave.
fn with_excludes(argv: &[String], rg: bool) -> Vec<String> {
    let mut extra: Vec<String> = Vec::new();
    for n in SECRET_NAMES {
        extra.push(if rg {
            format!("--glob=!{n}")
        } else {
            format!("--exclude={n}")
        });
    }
    for d in SECRET_DIRS.iter().filter_map(|d| d.rsplit('/').next()) {
        extra.push(if rg {
            format!("--glob=!{d}")
        } else {
            format!("--exclude-dir={d}")
        });
    }
    let at = argv.iter().position(|a| a == "--").unwrap_or(argv.len());
    let mut out = argv[..at].to_vec();
    out.extend(extra);
    out.extend_from_slice(&argv[at..]);
    out
}

/// Executable `name` found through PATH entries that are absolute and not
/// inside the working tree (a planted `./git` must never run).
pub fn resolve_program(name: &str, cwd: &str) -> Option<PathBuf> {
    resolve_in(name, cwd, &std::env::var_os("PATH").unwrap_or_default())
}

fn resolve_in(name: &str, cwd: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let cwd_real = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    safe_dirs_in(path, &cwd_real).into_iter().find_map(|d| {
        let p = d.join(name);
        let m = std::fs::metadata(&p).ok()?;
        (m.is_file() && m.permissions().mode() & 0o111 != 0).then_some(p)
    })
}

/// PATH entries that are safe to use from `cwd`.
pub fn safe_path_dirs(cwd_real: &Path) -> Vec<PathBuf> {
    safe_dirs_in(&std::env::var_os("PATH").unwrap_or_default(), cwd_real)
}

fn safe_dirs_in(path: &std::ffi::OsStr, cwd_real: &Path) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter(|d| d.is_absolute() && d.components().all(|c| c != Component::ParentDir))
        .filter(|d| {
            let real = std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
            // at `/` everything is "inside": system directories stay usable
            cwd_real == Path::new("/") || (!real.starts_with(cwd_real) && !d.starts_with(cwd_real))
        })
        .collect()
}

const LS: Spec = Spec {
    short: "aAbcdfFghHiklLmnNopqrRsStuUvxX1",
    long: &[
        "all",
        "almost-all",
        "human-readable",
        "si",
        "classify",
        "directory",
        "recursive",
        "reverse",
        "inode",
        "size",
        "dereference",
        "group-directories-first",
        "color",
        "full-time",
        "numeric-uid-gid",
        "literal",
        "hide-control-chars",
        "no-group",
        "sort",
        "time",
        "format",
    ],
    ..NONE
};

const CAT: Spec = Spec {
    short: "AbeEnstTuv",
    long: &[
        "show-all",
        "number-nonblank",
        "show-ends",
        "number",
        "squeeze-blank",
        "show-tabs",
        "show-nonprinting",
    ],
    ..NONE
};

const HEAD: Spec = Spec {
    short: "qvz",
    short_val: "nc",
    long: &["quiet", "silent", "verbose", "zero-terminated"],
    long_val: &["lines", "bytes"],
    digits: true,
};

const WC: Spec = Spec {
    short: "clmwL",
    long: &["lines", "words", "chars", "bytes", "max-line-length"],
    ..NONE
};

const STAT: Spec = Spec {
    short: "LfFt",
    short_val: "c",
    long: &["dereference", "file-system", "terse"],
    long_val: &["format", "printf"],
    digits: false,
};

const FILE: Spec = Spec {
    short: "bhiLnNsp",
    long: &[
        "brief",
        "mime",
        "mime-type",
        "mime-encoding",
        "dereference",
        "no-dereference",
    ],
    ..NONE
};

const DU: Spec = Spec {
    short: "abchHkmsxS",
    short_val: "dt",
    long: &[
        "all",
        "apparent-size",
        "bytes",
        "total",
        "human-readable",
        "si",
        "summarize",
        "one-file-system",
        "separate-dirs",
        "dereference",
        "count-links",
    ],
    long_val: &["max-depth", "threshold", "block-size"],
    digits: false,
};

const DF: Spec = Spec {
    short: "aHhikPTlm",
    short_val: "tx",
    long: &[
        "all",
        "human-readable",
        "si",
        "inodes",
        "local",
        "portability",
        "print-type",
        "total",
        "output",
    ],
    long_val: &["type", "exclude-type", "block-size"],
    digits: false,
};

const FREE: Spec = Spec {
    short: "bkmgthlLw",
    long: &[
        "bytes", "kilo", "mega", "giga", "tera", "human", "si", "lohi", "total", "wide",
    ],
    ..NONE
};

const UNAME: Spec = Spec {
    short: "asnrvmpio",
    long: &[
        "all",
        "kernel-name",
        "nodename",
        "kernel-release",
        "kernel-version",
        "machine",
        "processor",
        "hardware-platform",
        "operating-system",
    ],
    ..NONE
};

const PWD: Spec = Spec {
    short: "LP",
    ..NONE
};

const ID: Spec = Spec {
    short: "agGnruZ",
    long: &["group", "groups", "name", "real", "user", "context"],
    ..NONE
};

const WHICH: Spec = Spec {
    short: "a",
    long: &["all"],
    ..NONE
};

const DATE: Spec = Spec {
    short: "uR",
    short_val: "dr",
    long: &["utc", "universal", "rfc-email", "iso-8601"],
    long_val: &["date", "reference"],
    digits: false,
};

const GREP: Spec = Spec {
    short: "EFGiIvwxcLlonbHhqsaTUr",
    short_val: "eAmBCf",
    long: &[
        "extended-regexp",
        "fixed-strings",
        "basic-regexp",
        "ignore-case",
        "no-ignore-case",
        "invert-match",
        "word-regexp",
        "line-regexp",
        "count",
        "files-with-matches",
        "files-without-match",
        "only-matching",
        "line-number",
        "byte-offset",
        "with-filename",
        "no-filename",
        "quiet",
        "no-messages",
        "text",
        "recursive",
        "color",
        "colour",
        "null",
        "initial-tab",
        "binary-files",
    ],
    long_val: &[
        "regexp",
        "max-count",
        "after-context",
        "before-context",
        "context",
        "include",
        "exclude",
        "exclude-dir",
        "file",
    ],
    digits: false,
};

const RG: Spec = Spec {
    short: "iIsSvwxclnNHhqFuU",
    short_val: "eAmBCtgj",
    long: &[
        "ignore-case",
        "smart-case",
        "case-sensitive",
        "invert-match",
        "word-regexp",
        "line-regexp",
        "count",
        "files-with-matches",
        "files-without-match",
        "files",
        "only-matching",
        "line-number",
        "no-line-number",
        "with-filename",
        "no-filename",
        "fixed-strings",
        "hidden",
        "no-ignore",
        "quiet",
        "heading",
        "no-heading",
        "color",
        "no-messages",
        "column",
        "json",
    ],
    long_val: &[
        "regexp",
        "glob",
        "type",
        "max-count",
        "after-context",
        "before-context",
        "context",
        "max-depth",
        "threads",
    ],
    digits: false,
};

const PS: Spec = Spec {
    short: "eAaxfFjlLmMuwTvVHhcCNZ",
    short_val: "opUtgGqs",
    long: &[
        "forest",
        "no-headers",
        "headers",
        "cols",
        "columns",
        "lines",
        "width",
    ],
    long_val: &["format", "pid", "user", "sort", "ppid", "tty"],
    digits: false,
};

const SYSTEMCTL: Spec = Spec {
    short: "la",
    short_val: "n",
    long: &["full", "all", "no-pager", "no-legend", "user", "system"],
    long_val: &["lines", "output"],
    digits: false,
};

/// `status` of systemd, after the `--user`/`--system`/`--no-pager` words.
fn systemctl_ok(args: &[String]) -> bool {
    let n = args
        .iter()
        .take_while(|x| matches!(x.as_str(), "--user" | "--system" | "--no-pager"))
        .count();
    args.get(n).map(String::as_str) == Some("status") && parse(&SYSTEMCTL, &args[n + 1..]).is_some()
}

fn stage_plan(w: &[String]) -> Option<Stage> {
    let name = w.first()?.as_str();
    let args = &w[1..];
    let spec = match name {
        "ls" => &LS,
        "cat" => &CAT,
        "head" | "tail" => &HEAD,
        "wc" => &WC,
        "stat" => &STAT,
        "file" => &FILE,
        "du" => &DU,
        "df" => &DF,
        "free" => &FREE,
        "uname" => &UNAME,
        "whoami" => &NONE,
        "pwd" => &PWD,
        "id" => &ID,
        "which" => &WHICH,
        "date" => &DATE,
        "grep" => &GREP,
        "rg" => &RG,
        "ps" => return ps_ok(args).then(|| plain(w, None)),
        "find" => return find_stage(w),
        "git" => return Some(plain(&git_plan(args)?, None)),
        "systemctl" => return systemctl_ok(args).then(|| plain(w, None)),
        _ => return None,
    };
    let p = parse(spec, args)?;
    let rec = p.has("r") || p.has("recursive");
    Some(match name {
        "ls" => plain(w, (p.has("R") || p.has("recursive")).then_some(p.pos)),
        "du" => plain(w, Some(p.pos)),
        // a bare positional would set the clock
        "date" if !p.pos.iter().all(|x| x.starts_with('+')) => return None,
        "grep" if rec => Stage {
            argv: with_excludes(w, false),
            roots: Some(search_roots(&p)),
        },
        // rg always walks directories
        "rg" => Stage {
            argv: with_excludes(w, true),
            roots: Some(search_roots(&p)),
        },
        _ => plain(w, None),
    })
}

fn plain(argv: &[String], roots: Option<Vec<String>>) -> Stage {
    Stage {
        argv: argv.to_vec(),
        roots,
    }
}

/// Where grep/rg walk: the path arguments, or the cwd when only a pattern (or
/// `-e`/`-f` patterns) is given. The pattern word is counted as a path too.
fn search_roots(p: &Parsed) -> Vec<String> {
    let mut r = p.pos.clone();
    let patterns_by_flag = ["e", "f", "regexp", "file", "files"]
        .iter()
        .any(|f| p.has(f));
    if r.len() <= 1 || patterns_by_flag {
        r.push(".".into());
    }
    r
}

fn find_stage(w: &[String]) -> Option<Stage> {
    const FLAGS: &[&str] = &[
        "-H",
        "-L",
        "-P",
        "-print",
        "-print0",
        "-not",
        "-a",
        "-o",
        "-and",
        "-or",
        "-empty",
        "-prune",
        "-depth",
        "-xdev",
        "-mount",
        "-true",
        "-false",
        "-readable",
        "-executable",
        "-writable",
        "-ls",
    ];
    const VALS: &[&str] = &[
        "-name",
        "-iname",
        "-path",
        "-ipath",
        "-type",
        "-maxdepth",
        "-mindepth",
        "-size",
        "-mtime",
        "-atime",
        "-ctime",
        "-mmin",
        "-newer",
        "-user",
        "-group",
        "-perm",
        "-regex",
        "-iregex",
        "-lname",
        "-links",
        "-inum",
        "-uid",
        "-gid",
    ];
    let args = &w[1..];
    let mut roots = Vec::new();
    let mut i = 0;
    while i < args.len() && !args[i].starts_with('-') && args[i] != "!" {
        roots.push(args[i].clone());
        i += 1;
    }
    while i < args.len() {
        let a = args[i].as_str();
        i += 1;
        if a == "!" || a == "(" || a == ")" {
            if a == "!" {
                continue;
            }
            return None;
        }
        if FLAGS.contains(&a) {
            continue;
        }
        if VALS.contains(&a) {
            args.get(i)?;
            i += 1;
            continue;
        }
        return None;
    }
    Some(Stage {
        argv: w.to_vec(),
        roots: Some(roots),
    })
}

/// Existing path-like words, resolved: a symlink must not smuggle a secret.
fn paths_ok(words: &[String], cwd: &str) -> bool {
    // path-like parts of a word: itself, `--opt=VALUE`, `-fVALUE`, `rev:path`
    let mut parts: Vec<String> = Vec::new();
    for w in words {
        parts.push(w.clone());
        if w.starts_with('-') {
            if let Some((_, v)) = w.split_once('=') {
                parts.push(v.to_owned());
            }
            if !w.starts_with("--") && w.chars().count() > 2 {
                parts.push(w.chars().skip(2).collect());
            }
        }
        if let Some((_, v)) = w.split_once(':') {
            parts.push(v.to_owned());
        }
        if let Some((_, v)) = w.rsplit_once(':') {
            parts.push(v.to_owned());
        }
    }
    parts.iter().all(|w| {
        if is_secret_path(w) {
            return false;
        }
        let real = resolve(w, cwd);
        !(is_secret_path(&real.to_string_lossy()) || via_proc_pid(w, cwd))
    })
}

/// A plan for `cmd` if it only reads, `None` otherwise. `extra`: the config's
/// `allow:` prefixes (trusted, but still shell-free and secret-checked).
pub fn plan(cmd: &str, extra: &[String], cwd: &str) -> Option<Plan> {
    let (toks, odd) = lex(cmd)?;
    if odd {
        return None;
    }
    let extra: Vec<Vec<String>> = extra
        .iter()
        .map(|e| e.split_whitespace().map(str::to_owned).collect())
        .collect();
    let mut stages: Vec<Vec<String>> = vec![Vec::new()];
    let mut toks = toks.into_iter();
    while let Some(t) = toks.next() {
        match t {
            Tok::Word(w) => {
                if w.expands || w.glob {
                    return None;
                }
                let text = if w.tilde {
                    if w.text != "~" && !w.text.starts_with("~/") {
                        return None;
                    }
                    expand_tilde(&w.text)
                } else {
                    w.text
                };
                stages.last_mut()?.push(text);
            }
            Tok::Pipe { amp: false } => stages.push(Vec::new()),
            // only `2>&1` (stderr is merged into the output anyway)
            Tok::Out {
                dup: true,
                fd: Some(fd),
                clean: true,
            } if fd == "2" && matches!(toks.next(), Some(Tok::Word(w)) if w.text == "1") => {}
            _ => return None,
        }
    }
    let mut out = Vec::new();
    for words in stages {
        let first = words.first()?;
        if first.contains(['=', '/']) || first.starts_with('-') {
            return None;
        }
        if !paths_ok(&words, cwd) {
            return None;
        }
        let trusted = extra
            .iter()
            .any(|p| !p.is_empty() && words.len() >= p.len() && words[..p.len()] == p[..]);
        if trusted {
            out.push(words);
            continue;
        }
        let st = stage_plan(&words)?;
        if let Some(roots) = st.roots {
            let roots = if roots.is_empty() {
                vec![".".to_owned()]
            } else {
                roots
            };
            if roots.iter().any(|r| root_too_wide(&resolve(r, cwd))) {
                return None;
            }
        }
        out.push(st.argv);
    }
    Some(Plan { stages: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(c: &str) -> bool {
        // a cwd that is neither / nor $HOME, so recursive searches are judged
        plan(c, &[], "/tmp/proj-x").is_some()
    }

    #[test]
    fn allows_plain_reads() {
        for c in [
            "ls",
            "ls -la /tmp",
            "cat Cargo.toml",
            "head -n 5 src/main.rs",
            "head -5 f",
            "tail -n20 log.txt",
            "wc -l a b",
            "grep -n 'foo bar' f",
            "grep -rn \"TODO\" src",
            "rg -n TODO src",
            "find . -name '*.rs' -type f",
            "find src -maxdepth 2 -iname \"*.md\"",
            "stat file",
            "du -sh src",
            "df -h",
            "free -m",
            "uname -a",
            "whoami",
            "id",
            "ps aux",
            "ps -ef",
            "ps -o pid,comm",
            "which cargo",
            "pwd",
            "date +%s",
            "git status",
            "git status -sb",
            "git log --oneline -5",
            "git log -5",
            "git diff --cached",
            "git branch -a",
            "git remote -v",
            "git rev-parse --show-toplevel",
            "git --no-pager log --oneline",
            "systemctl status sshd",
            "systemctl --user status foo",
            "ls | head -3",
            "ls 2>&1 | grep x",
            "cat \"a b\"",
            "ls ~/projects",
        ] {
            assert!(ok(c), "{c}");
        }
    }

    #[test]
    fn rejects_mutation_and_escapes() {
        for c in [
            "",
            "rm -rf /",
            "ls; rm x",
            "ls\nrm x",
            "ls & rm x",
            "ls > out.txt",
            "ls >> out.txt",
            "ls > /dev/null",
            "cat < in",
            "cat <<EOF",
            "cat <(ls)",
            "ls $(rm x)",
            "ls `rm x`",
            "ls \"$(rm x)\"",
            "ls ${HOME}",
            "ls $HOME",
            "FOO=1 ls",
            "ls | sh",
            "ls | xargs rm",
            "ls && pwd",
            "ls || pwd",
            "ls | tee f",
            "tail -f log",
            "tail -F log",
            "tail -fn5 log",
            "tail --follow log",
            "find . -exec rm {} ;",
            "find . -delete",
            "find . -fprint out",
            "git push",
            "git commit -m x",
            "git reset --hard",
            "git branch -d x",
            "git branch -vd x",
            "git branch newname",
            "git remote add x y",
            "git -c core.pager=sh log",
            "git diff --output=f",
            "git",
            "cargo build",
            "cargo metadata",
            "systemctl restart sshd",
            "journalctl --vacuum-size=1M",
            "journalctl -f",
            "journalctl -fu x",
            "rg --pre sh x",
            "date -s now",
            "date 010112002026",
            "sudo ls",
            "/bin/ls",
            "./ls",
            "ls (",
            "ls |& cat",
            "ls 2>file",
            "ls '",
            "ls \"",
            "ls | ",
            "| ls",
            "ls |",
            "type ls",
        ] {
            assert!(!ok(c), "{c:?}");
        }
    }

    #[test]
    fn review_1_ansi_c_quoting_and_dollar() {
        for c in [
            "git diff $'--output=~/.bashrc'",
            "find . $'-delete'",
            "cat ~/$'\\x2essh'/id_ed25519",
            "cat $\"x\"",
            "echo $'x'",
            "ls \"$HOME\"",
        ] {
            assert!(!ok(c), "{c:?}");
        }
        // single-quoted `$` is a plain character
        assert!(ok("grep '$HOME' f"));
    }

    #[test]
    fn review_2_brace_expansion() {
        for c in ["git diff --{output=/tmp/x,stat}", "ls {a,b}", "ls a{b"] {
            assert!(!ok(c), "{c:?}");
        }
        assert!(ok("grep '{a,b}' f"));
    }

    #[test]
    fn review_3_globs_and_option_injection() {
        for c in [
            "cat ~/.ss?/id_*",
            "head ~/.config/*/keys.env",
            "git diff *",
            "find *",
            "ls [a]*",
            "wc -l *.rs",
        ] {
            assert!(!ok(c), "{c:?}");
        }
        // quoted: literal pattern, handed to the program as is, no shell
        assert!(ok("find . -name '*.rs'"));
        // no shell: quoting is ours; the argv is exact
        let p = plan("grep 'a b' \"c d\" | head -2", &[], "/tmp/x").unwrap();
        assert_eq!(p.stages[0], ["grep", "a b", "c d"]);
        assert_eq!(p.stages.len(), 2);
    }

    #[test]
    fn review_4_secret_paths_and_wide_recursion() {
        for p in [
            "~/.ssh/id_rsa",
            "/home/u/.ssh/config",
            "key.PEM",
            "a.key",
            "x.p12",
            "x.pfx",
            ".env",
            "a/.env.prod",
            "prod.env",
            "/root/.aws/credentials",
            "~/.aws/config",
            "~/.netrc",
            "~/.git-credentials",
            "~/.npmrc",
            "~/.pypirc",
            "~/.pgpass",
            "~/.kube/config",
            "~/.docker/config.json",
            "~/.config/gh/hosts.yml",
            "~/.config/gcloud/credentials.db",
            "~/.gnupg/pubring.kbx",
            "~/.password-store/x.gpg",
            "~/.bash_history",
            "~/.zsh_history",
            "~/.python_history",
            "~/.psql_history",
            "~/.mysql_history",
            "~/.node_repl_history",
            "/proc/1/environ",
            "/proc/self/cmdline",
            "/proc/1/mem",
            "/proc/1/maps",
            "/etc/shadow",
            "/etc/shadow-",
            "/etc/sudoers",
            "/etc/sudoers.d/x",
            "~/.config/aider/x",
            "/h/.config/aish/config.yaml",
            "any/dir/id_ed25519",
        ] {
            assert!(is_secret_path(p), "{p}");
        }
        for p in [
            "src/main.rs",
            "environment.txt",
            "/etc/hosts",
            "a.pem.txt",
            "/proc/1/status",
        ] {
            assert!(!is_secret_path(p), "{p}");
        }
        let h = home();
        for c in [
            "grep -r x /".to_owned(),
            "grep -r x ~".to_owned(),
            "rg x /".to_owned(),
            "rg x ~".to_owned(),
            "find / -name x".to_owned(),
            if h.is_empty() {
                "find / -name y".to_owned()
            } else {
                format!("find {h} -name x")
            },
            "du -sh /".to_owned(),
            "du ~".to_owned(),
            "ls -R /".to_owned(),
            "ls -R ~".to_owned(),
            "find /proc".to_owned(),
            "cat ~/.ssh/id_rsa".to_owned(),
            "cat ./../.ssh/id_rsa".to_owned(),
        ] {
            assert!(plan(&c, &[], "/tmp/x").is_none(), "{c}");
        }
        // the default root is the cwd
        assert!(plan("rg x", &[], "/").is_none());
        if !h.is_empty() {
            assert!(plan("grep -r x", &[], &h).is_none());
        }
        assert!(plan("rg x", &[], "/tmp/x").is_some());
        assert!(plan("ls /", &[], "/tmp/x").is_some()); // not recursive
    }

    #[test]
    fn review_5_ps_environment() {
        for c in [
            "ps e",
            "ps auxe",
            "ps eww",
            "ps -o pid,environ",
            "ps -o env",
            "ps --format=args,environ",
            "ps --format env",
            "ps -eo pid,environ",
        ] {
            assert!(!ok(c), "{c:?}");
        }
        assert!(ok("ps -e"));
        assert!(ok("ps -ef --forest"));
    }

    #[test]
    fn review_6_no_option_abbreviations() {
        for c in [
            "tail --fol log",
            "tail --foll log",
            "file --comp x",
            "journalctl --rot",
            "journalctl --vac=1",
            "date --se=now",
            "git diff --outp=f",
            "git log --ext",
            "grep --recurs x .",
            "ls --recu",
            "find . -exe rm {} ;",
            "find . -del",
        ] {
            assert!(!ok(c), "{c:?}");
        }
        assert!(ok("ls --all"));
        assert!(ok("ls -la --color=auto"));
        assert!(ok("head --lines=5 f"));
        assert!(ok("head --lines 5 f"));
        assert!(!ok("head --unknown f"));
    }

    #[test]
    fn review_7_systemctl_is_local_and_cargo_is_not_run() {
        for c in [
            "systemctl status -H user@host x",
            "systemctl status --host=h x",
            "systemctl status -M m x",
            "systemctl status --machine=m x",
            "cargo metadata --offline --no-deps",
            "journalctl -n 5",
        ] {
            assert!(!ok(c), "{c:?}");
        }
    }

    #[test]
    fn review_8_git_is_neutered() {
        let p = plan("git diff --stat", &[], "/tmp/x").unwrap();
        assert_eq!(
            p.stages[0],
            [
                "git",
                "--no-pager",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--stat"
            ]
        );
        let p = plan("git status", &[], "/tmp/x").unwrap();
        assert_eq!(p.stages[0], ["git", "--no-pager", "status"]);
    }

    #[test]
    fn config_prefixes_extend_the_list() {
        let extra = vec!["make check".to_owned(), "docker ps".to_owned()];
        let p = |c: &str| plan(c, &extra, "/tmp/x").is_some();
        assert!(p("make check -j4"));
        assert!(p("docker ps -a | head"));
        assert!(!p("make install"));
        assert!(!p("make check; rm x"));
        assert!(!p("make check $(x)"));
        assert!(!p("docker rm x"));
        assert!(!p("make check ~/.ssh/id_rsa"));
    }

    #[test]
    fn tilde_is_expanded_only_when_unquoted_and_plain() {
        assert!(!ok("ls ~root"));
        let p = plan("ls ~/x", &[], "/tmp/x").unwrap();
        assert!(Path::new(&p.stages[0][1]).is_absolute() || home().is_empty());
        let p = plan("ls '~/x'", &[], "/tmp/x").unwrap();
        assert_eq!(p.stages[0][1], "~/x");
    }

    #[test]
    fn round2_recursive_search_excludes_secret_names_last() {
        let p = plan("grep -r --include='*' KEY .", &[], "/tmp/x").unwrap();
        let a = &p.stages[0];
        let inc = a.iter().position(|x| x == "--include=*").unwrap();
        let ex = a.iter().position(|x| x == "--exclude=.env*").unwrap();
        assert!(ex > inc, "excludes come after the user's --include: {a:?}");
        assert!(
            a.contains(&"--exclude-dir=.ssh".to_owned())
                && a.contains(&"--exclude=id_*".to_owned())
        );
        assert!(
            a.contains(&"KEY".to_owned()) && a.contains(&".".to_owned()),
            "{a:?}"
        );
        // before a `--`, never after it
        let p = plan("grep -r -- KEY .", &[], "/tmp/x").unwrap();
        let a = &p.stages[0];
        let dd = a.iter().position(|x| x == "--").unwrap();
        assert!(a.iter().position(|x| x == "--exclude=.env*").unwrap() < dd);
        // rg: always, `-uu` and --hidden keep working but the excludes win
        for c in [
            "rg KEY",
            "rg -uu KEY .",
            "rg --hidden --no-ignore KEY src",
            "rg -g '*.rs' KEY",
        ] {
            let p = plan(c, &[], "/tmp/x").unwrap();
            let a = &p.stages[0];
            assert!(
                a.contains(&"--glob=!.env*".to_owned()) && a.contains(&"--glob=!.ssh".to_owned()),
                "{c}: {a:?}"
            );
            let last_glob = a.iter().rposition(|x| x.starts_with("--glob=")).unwrap();
            assert!(
                a.iter()
                    .take(last_glob)
                    .all(|x| x != "KEY" || c == "rg KEY")
                    || last_glob > 0
            );
        }
        // non-recursive grep of named files is not rewritten
        assert_eq!(
            plan("grep x a.txt", &[], "/tmp/x").unwrap().stages[0],
            ["grep", "x", "a.txt"]
        );
    }

    #[test]
    fn round2_no_symlink_following_searches() {
        for c in [
            "grep -R x .",
            "grep --dereference-recursive x .",
            "rg -L x",
            "rg --follow x",
            "rg -uuL x",
            "du -L .",
        ] {
            assert!(!ok(c), "{c}");
        }
        assert!(ok("grep -r x ."));
    }

    #[test]
    fn round2_program_lookup_ignores_relative_and_cwd_entries() {
        let d = std::env::temp_dir().join(format!("aish-pl-{}", std::process::id()));
        let tree = d.join("proj");
        std::fs::create_dir_all(tree.join("bin")).unwrap();
        let fake = tree.join("bin/git");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let ok_dir = d.join("trusted");
        std::fs::create_dir_all(&ok_dir).unwrap();
        let real = ok_dir.join("git");
        std::fs::write(&real, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&real, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let path = std::env::join_paths([
            PathBuf::from(""),
            PathBuf::from("."),
            PathBuf::from("bin"),
            tree.join("bin"),
            tree.join("bin/../bin"),
            ok_dir.clone(),
        ])
        .unwrap();
        let cwd = tree.to_str().unwrap();
        assert_eq!(resolve_in("git", cwd, &path), Some(real));
        let only_bad = std::env::join_paths([PathBuf::from("."), tree.join("bin")]).unwrap();
        assert_eq!(resolve_in("git", cwd, &only_bad), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn round2_glued_values_and_rev_path_are_secret_checked() {
        for c in [
            "grep --file=id_rsa x",
            "grep --file=~/.ssh/known x",
            "grep -f.env x",
            "grep -fid_ed25519 x",
            "git show HEAD:.env",
            "git show HEAD:~/.ssh/id_rsa",
            "git show HEAD:config/prod.env",
            "git show HEAD~2:id_rsa",
        ] {
            assert!(!ok(c), "{c}");
        }
        assert!(ok("grep --file=pats.txt x"));
    }

    /// Bypasses found in review (two probe sets): nothing here may run unasked.
    #[test]
    fn reviewed_probes_are_not_read_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let w = dir.path();
        std::fs::create_dir_all(w.join("repo/.git")).unwrap();
        std::fs::create_dir_all(w.join("empty")).unwrap();
        std::fs::write(w.join("repo/a"), "x").unwrap();
        let cwd = w.join("repo").to_string_lossy().into_owned();
        let empty = w.join("empty").to_string_lossy().into_owned();
        let link = |to: &str, name: &str| {
            std::os::unix::fs::symlink(to, w.join("repo").join(name)).unwrap();
        };
        link("/proc/self/environ", "notes.txt");
        link(&empty, "outside");
        let ssh = w.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("key"), "k").unwrap();
        link(&ssh.join("key").to_string_lossy(), "readme.txt");
        let bad = [
            "git diff ~ EMPTY".to_owned(),
            "git diff --stat ~ EMPTY".to_owned(),
            "git diff EMPTY".to_owned(),
            "cat notes.txt".to_owned(),
            "cat readme.txt".to_owned(),
            "head -c 9 /proc/self/environ".to_owned(),
            "grep -e -- -r -e '' ~".to_owned(),
            "grep -ve -- -r ~".to_owned(),
            "grep -e -- -r '' /proc/1".to_owned(),
            "diff -rN ~ EMPTY".to_owned(),
            "setarch x86_64 rm -rf x".to_owned(),
            "prlimit rm -rf x".to_owned(),
            "env rm x".to_owned(),
            "git -c core.pager=x log".to_owned(),
            "git log --output=x".to_owned(),
            "find . -fprint x".to_owned(),
            format!("cat /proc/{}/root/etc/hostname", std::process::id()),
            format!("cat /proc/{}/cwd/a", std::process::id()),
            format!("grep -r x /proc/{}/root/etc", std::process::id()),
            "cat /dev/fd/../../1/root/etc/hostname".to_owned(),
            "cat -\u{e9}".to_owned(),
            "git log -\u{663}".to_owned(),
        ];
        for c in bad {
            let c = c.replace("EMPTY", &empty);
            assert!(plan(&c, &[], &cwd).is_none(), "accepted: {c}");
        }
        for c in ["git log --oneline -5", "git status --short", "cat a"] {
            assert!(plan(c, &[], &cwd).is_some(), "refused: {c}");
        }
    }

    /// git takes `-U`'s value (and `--branches`, ...) only attached: no word after
    /// them may be skipped as a value, so no option is parsed at all.
    #[test]
    fn git_runs_only_whole_listed_commands() {
        for c in [
            "git log -1 '--format=tformat:touch x' -U --output=~/.bashrc",
            "git log -U --ext-diff",
            "git diff -U --textconv",
            "git log --branches --output=x",
            "git log --tags --output=x",
            "git log --oneline --output=x",
            "git log --oneline -n 5",
            "git log -1x",
            "git log --",
            "git status -s x",
            "git diff HEAD",
            "git show HEAD",
            "git branch -D x",
            "git remote add x y",
            "git",
        ] {
            assert!(!ok(c), "{c:?}");
        }
    }

    #[test]
    fn an_optional_value_is_not_swallowed() {
        // df takes --output's list only attached: `-o` is a flag we do not know
        assert!(ok("df --output -h"));
        assert!(!ok("df --output -o"));
    }

    #[test]
    fn paths_are_resolved_as_the_kernel_does() {
        let pid = std::process::id();
        let cwd = "/tmp/proj-x";
        for c in [
            format!("cat /dev/fd/../../{pid}/environ"),
            format!("grep -r tok /dev/fd/../../{pid}"),
            format!("ls /proc/{pid}/fd"),
            "cat /proc/self/environ".to_owned(),
        ] {
            assert!(plan(&c, &[], cwd).is_none(), "accepted: {c}");
        }
        assert!(plan("cat /proc/cpuinfo", &[], cwd).is_some());
    }
}

#[cfg(test)]
mod frozen;
