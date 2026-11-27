//! The read-only allowlist: which commands the agent may run by itself.
//!
//! A command line is tokenized like a shell would (quotes, escapes) and turned
//! into a *plan*: a pipeline of argv vectors that aish executes itself with
//! no shell involved, so quoting is only our tokenizer's job. Every simple
//! command must be on the list and every option must be a known safe one
//! (exact names: no GNU abbreviations). Anything not understood is "not
//! read-only": the caller then hands the command to the user instead.
//! When in doubt: refuse. A false negative costs one Enter.

use std::path::{Component, Path, PathBuf};

/// What to execute: stages joined by pipes, argv each.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub stages: Vec<Vec<String>>,
}

/// A word with its quoting facts.
#[derive(Debug)]
struct Word {
    text: String,
    /// Unquoted `*?[`.
    glob: bool,
    /// Starts with an unquoted `~`.
    tilde: bool,
}

enum Tok {
    Word(Word),
    Pipe,
}

/// Tokenize; `None` = something we refuse to reason about: any `$` outside
/// single quotes (expansions, `$'..'`), backticks, `;`, `&`, newlines,
/// subshells, unquoted braces, redirections (but `2>&1`), `&&`/`||`.
fn tokenize(s: &str) -> Option<Vec<Tok>> {
    let cs: Vec<char> = s.chars().collect();
    let mut toks = Vec::new();
    let mut w: Option<Word> = None;
    let mut i = 0;
    fn cur(w: &mut Option<Word>) -> &mut Word {
        w.get_or_insert(Word {
            text: String::new(),
            glob: false,
            tilde: false,
        })
    }
    while i < cs.len() {
        let c = cs[i];
        match c {
            '\'' => {
                let word = cur(&mut w);
                i += 1;
                loop {
                    match cs.get(i)? {
                        '\'' => break,
                        &x => word.text.push(x),
                    }
                    i += 1;
                }
            }
            '"' => {
                let word = cur(&mut w);
                i += 1;
                loop {
                    match cs.get(i)? {
                        '"' => break,
                        '`' | '$' => return None,
                        '\\' => {
                            i += 1;
                            match *cs.get(i)? {
                                x @ ('"' | '\\') => word.text.push(x),
                                x => {
                                    word.text.push('\\');
                                    word.text.push(x);
                                }
                            }
                        }
                        &x => word.text.push(x),
                    }
                    i += 1;
                }
            }
            '\\' => {
                i += 1;
                let x = *cs.get(i)?;
                if x == '\n' {
                    return None;
                }
                cur(&mut w).text.push(x);
            }
            '`' | '$' | '\n' | ';' | '(' | ')' | '{' | '}' | '<' => return None,
            ' ' | '\t' => {
                if let Some(x) = w.take() {
                    toks.push(Tok::Word(x));
                }
            }
            '|' => {
                if cs.get(i + 1) == Some(&'|') || cs.get(i + 1) == Some(&'&') {
                    return None;
                }
                if let Some(x) = w.take() {
                    toks.push(Tok::Word(x));
                }
                toks.push(Tok::Pipe);
            }
            '&' => return None,
            '>' => {
                // only `2>&1` (stderr is merged into the output anyway)
                let is_fd = w.as_ref().is_some_and(|x| x.text == "2");
                let rest: String = cs[i..].iter().take(3).collect();
                if is_fd && rest == ">&1" && cs.get(i + 3).is_none_or(|x| x.is_whitespace()) {
                    w = None;
                    i += 3;
                    continue;
                }
                return None;
            }
            '*' | '?' | '[' => {
                let x = cur(&mut w);
                x.glob = true;
                x.text.push(c);
            }
            '~' if w.is_none() => {
                let x = cur(&mut w);
                x.tilde = true;
                x.text.push(c);
            }
            _ => cur(&mut w).text.push(c),
        }
        i += 1;
    }
    if let Some(x) = w.take() {
        toks.push(Tok::Word(x));
    }
    Some(toks)
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
    let has = |d: &str| parts.iter().any(|x| x == d);
    if [".ssh", ".gnupg", ".aws", ".password-store", ".kube"]
        .iter()
        .any(|d| has(d))
    {
        return true;
    }
    // ~/.config/{gcloud,gh,aider,aish}/..
    if parts
        .windows(2)
        .any(|w| w[0] == ".config" && ["gcloud", "gh", "aider", "aish"].contains(&w[1].as_str()))
    {
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
        || (has(".docker") && base == "config.json")
        || (e.ends_with(".config/aish/config.yaml"))
}

/// Lexical normalisation (`.`/`..`), `~` expanded, relative to `cwd`.
fn absolute(p: &str, cwd: &str) -> PathBuf {
    let e = expand_tilde(p);
    let joined = if Path::new(&e).is_absolute() {
        PathBuf::from(e)
    } else {
        Path::new(cwd).join(e)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// A recursive search from `root` would walk into secret directories.
fn root_too_wide(root: &Path) -> bool {
    let h = home();
    let mut dangerous: Vec<PathBuf> = vec![
        PathBuf::from("/etc/sudoers.d"),
        PathBuf::from("/proc"),
        PathBuf::from("/root"),
    ];
    if !h.is_empty() {
        dangerous.push(PathBuf::from(&h));
        for d in [
            ".ssh",
            ".gnupg",
            ".aws",
            ".kube",
            ".password-store",
            ".docker",
            ".config/gcloud",
            ".config/gh",
            ".config/aider",
            ".config/aish",
        ] {
            dangerous.push(Path::new(&h).join(d));
        }
    }
    // the root itself, or any ancestor of a dangerous directory
    dangerous.iter().any(|d| d.starts_with(root))
        || std::fs::canonicalize(root)
            .map(|r| dangerous.iter().any(|d| d.starts_with(&r)))
            .unwrap_or(false)
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

const GIT_LOG_LONG: &[&str] = &[
    "stat",
    "shortstat",
    "numstat",
    "name-only",
    "name-status",
    "oneline",
    "pretty",
    "format",
    "graph",
    "decorate",
    "all",
    "no-merges",
    "merges",
    "first-parent",
    "abbrev-commit",
    "abbrev",
    "cached",
    "staged",
    "no-color",
    "color",
    "patch",
    "no-patch",
    "summary",
    "raw",
    "unified",
    "word-diff",
    "check",
    "reverse",
    "date",
    "follow",
    "relative",
    "minimal",
    "histogram",
    "patience",
    "ignore-space-change",
    "ignore-all-space",
    "no-ext-diff",
    "no-textconv",
    "topo-order",
    "date-order",
    "parents",
    "children",
    "left-right",
    "cherry-pick",
    "stat-width",
    "compact-summary",
    "dirstat",
    "full-index",
    "find-renames",
    "no-renames",
];
const GIT_LOG_VAL: &[&str] = &[
    "since",
    "until",
    "author",
    "committer",
    "grep",
    "max-count",
    "skip",
    "diff-filter",
    "branches",
    "tags",
    "remotes",
];

fn git_plan(args: &[String]) -> Option<Vec<String>> {
    let mut a = args.iter().peekable();
    while a.peek().is_some_and(|x| *x == "--no-pager") {
        a.next();
    }
    let sub = a.next()?.clone();
    let rest: Vec<String> = a.cloned().collect();
    let mut out = vec!["git".to_owned(), "--no-pager".to_owned(), sub.clone()];
    let ok = match sub.as_str() {
        "log" | "diff" | "show" => {
            out.push("--no-ext-diff".into());
            out.push("--no-textconv".into());
            parse(
                &Spec {
                    short: "pbwsuMCa",
                    short_val: "nU",
                    long: GIT_LOG_LONG,
                    long_val: GIT_LOG_VAL,
                    digits: true,
                },
                &rest,
            )
            .is_some()
        }
        "status" => parse(
            &Spec {
                short: "sbznvV",
                short_val: "u",
                long: &[
                    "short",
                    "branch",
                    "porcelain",
                    "long",
                    "ignored",
                    "untracked-files",
                    "ahead-behind",
                    "no-ahead-behind",
                    "renames",
                    "no-renames",
                    "show-stash",
                    "verbose",
                    "ignore-submodules",
                    "column",
                    "no-column",
                ],
                ..NONE
            },
            &rest,
        )
        .is_some(),
        "rev-parse" => parse(
            &Spec {
                short: "q",
                long: &[
                    "short",
                    "abbrev-ref",
                    "show-toplevel",
                    "git-dir",
                    "is-inside-work-tree",
                    "verify",
                    "symbolic",
                    "symbolic-full-name",
                    "show-prefix",
                    "show-cdup",
                    "absolute-git-dir",
                    "git-common-dir",
                    "is-bare-repository",
                    "quiet",
                    "sq",
                ],
                ..NONE
            },
            &rest,
        )
        .is_some(),
        "ls-files" => parse(
            &Spec {
                short: "cdmoiskuz",
                long: &[
                    "cached",
                    "deleted",
                    "modified",
                    "others",
                    "ignored",
                    "stage",
                    "killed",
                    "unmerged",
                    "exclude-standard",
                    "full-name",
                    "error-unmatch",
                    "directory",
                    "no-empty-directory",
                    "eol",
                    "deduplicate",
                    "abbrev",
                ],
                ..NONE
            },
            &rest,
        )
        .is_some(),
        "blame" => parse(
            &Spec {
                short: "bewfnpstl",
                short_val: "L",
                long: &[
                    "porcelain",
                    "line-porcelain",
                    "incremental",
                    "show-name",
                    "show-number",
                    "show-email",
                    "root",
                    "date",
                ],
                ..NONE
            },
            &rest,
        )
        .is_some(),
        "branch" => parse(
            &Spec {
                short: "arlvV",
                long: &[
                    "all",
                    "remotes",
                    "verbose",
                    "list",
                    "show-current",
                    "color",
                    "no-color",
                    "abbrev",
                    "sort",
                    "format",
                    "column",
                ],
                long_val: &[
                    "contains",
                    "no-contains",
                    "merged",
                    "no-merged",
                    "points-at",
                ],
                ..NONE
            },
            &rest,
        )
        .is_some_and(|p| {
            // a bare name would create a branch
            p.pos.is_empty()
                || p.has("list")
                || p.has("l")
                || p.has("contains")
                || p.has("merged")
                || p.has("no-merged")
                || p.has("points-at")
                || p.has("no-contains")
        }),
        "remote" => rest.is_empty() || rest == ["-v"] || rest == ["--verbose"],
        _ => false,
    };
    if !ok {
        return None;
    }
    // positional args (paths, revs) go after the injected flags
    out.extend(rest);
    Some(out)
}

fn ps_ok(args: &[String]) -> bool {
    let spec = Spec {
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
    let Some(p) = parse(&spec, rest) else {
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
const SECRET_DIR_NAMES: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".password-store",
    ".kube",
    ".docker",
    "gcloud",
    "aider",
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
    for d in SECRET_DIR_NAMES {
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

fn stage_plan(w: &[String]) -> Option<Stage> {
    let name = w.first()?.as_str();
    let args = &w[1..];
    let simple = |spec: &Spec| -> Option<Parsed> { parse(spec, args) };
    let plain = |p: Option<Parsed>| -> Option<Stage> {
        p.map(|_| Stage {
            argv: w.to_vec(),
            roots: None,
        })
    };
    match name {
        "ls" => {
            let p = simple(&Spec {
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
            })?;
            let rec = p.has("R") || p.has("recursive");
            Some(Stage {
                argv: w.to_vec(),
                roots: rec.then(|| p.pos.clone()),
            })
        }
        "cat" => plain(simple(&Spec {
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
        })),
        "head" | "tail" => plain(simple(&Spec {
            short: "qvz",
            short_val: "nc",
            long: &["quiet", "silent", "verbose", "zero-terminated"],
            long_val: &["lines", "bytes"],
            digits: true,
        })),
        "wc" => plain(simple(&Spec {
            short: "clmwL",
            long: &["lines", "words", "chars", "bytes", "max-line-length"],
            ..NONE
        })),
        "stat" => plain(simple(&Spec {
            short: "LfFt",
            short_val: "c",
            long: &["dereference", "file-system", "terse"],
            long_val: &["format", "printf"],
            digits: false,
        })),
        "file" => plain(simple(&Spec {
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
        })),
        "du" => {
            let p = simple(&Spec {
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
            })?;
            Some(Stage {
                argv: w.to_vec(),
                roots: Some(p.pos),
            })
        }
        "df" => plain(simple(&Spec {
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
            ],
            long_val: &["type", "exclude-type", "output", "block-size"],
            digits: false,
        })),
        "free" => plain(simple(&Spec {
            short: "bkmgthlLw",
            long: &[
                "bytes", "kilo", "mega", "giga", "tera", "human", "si", "lohi", "total", "wide",
            ],
            ..NONE
        })),
        "uname" => plain(simple(&Spec {
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
        })),
        "whoami" => plain(simple(&NONE)),
        "pwd" => plain(simple(&Spec {
            short: "LP",
            ..NONE
        })),
        "id" => plain(simple(&Spec {
            short: "agGnruZ",
            long: &["group", "groups", "name", "real", "user", "context"],
            ..NONE
        })),
        "which" => plain(simple(&Spec {
            short: "a",
            long: &["all"],
            ..NONE
        })),
        "date" => {
            let p = simple(&Spec {
                short: "uR",
                short_val: "dr",
                long: &["utc", "universal", "rfc-email", "iso-8601"],
                long_val: &["date", "reference"],
                digits: false,
            })?;
            // a bare positional would set the clock
            p.pos.iter().all(|x| x.starts_with('+')).then(|| Stage {
                argv: w.to_vec(),
                roots: None,
            })
        }
        "ps" => ps_ok(args).then(|| Stage {
            argv: w.to_vec(),
            roots: None,
        }),
        "grep" => {
            let p = simple(&Spec {
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
            })?;
            let rec = p.has("r") || p.has("recursive");
            Some(Stage {
                argv: if rec {
                    with_excludes(w, false)
                } else {
                    w.to_vec()
                },
                roots: rec.then(|| search_roots(&p)),
            })
        }
        "rg" => {
            let p = simple(&Spec {
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
            })?;
            // rg always walks directories
            Some(Stage {
                argv: with_excludes(w, true),
                roots: Some(search_roots(&p)),
            })
        }
        "find" => find_stage(w),
        "git" => Some(Stage {
            argv: git_plan(args)?,
            roots: None,
        }),
        "cargo" => {
            if args.first().map(String::as_str) != Some("metadata") {
                return None;
            }
            let p = parse(
                &Spec {
                    long: &["offline", "frozen", "no-deps", "locked"],
                    long_val: &[
                        "format-version",
                        "manifest-path",
                        "features",
                        "filter-platform",
                    ],
                    ..NONE
                },
                &args[1..],
            )?;
            // no network: resolving may fetch (and run build scripts of nothing)
            (p.has("offline") || p.has("frozen")).then(|| Stage {
                argv: w.to_vec(),
                roots: None,
            })
        }
        "systemctl" => {
            let mut i = 0;
            while args
                .get(i)
                .is_some_and(|x| matches!(x.as_str(), "--user" | "--system" | "--no-pager"))
            {
                i += 1;
            }
            if args.get(i).map(String::as_str) != Some("status") {
                return None;
            }
            plain(parse(
                &Spec {
                    short: "la",
                    short_val: "n",
                    long: &["full", "all", "no-pager", "no-legend", "user", "system"],
                    long_val: &["lines", "output"],
                    digits: false,
                },
                &args[i + 1..],
            ))
        }
        "journalctl" => plain(simple(&Spec {
            short: "xbeaqrlkm",
            short_val: "unopSUtgc",
            long: &[
                "no-pager",
                "reverse",
                "kernel",
                "dmesg",
                "boot",
                "catalog",
                "all",
                "quiet",
                "utc",
                "no-hostname",
                "full",
                "system",
                "user",
                "this-boot",
            ],
            long_val: &[
                "unit",
                "user-unit",
                "lines",
                "output",
                "priority",
                "since",
                "until",
                "identifier",
                "grep",
                "case-sensitive",
                "cursor",
                "after-cursor",
            ],
            digits: false,
        })),
        _ => None,
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
            if !w.starts_with("--") && w.len() > 2 {
                parts.push(w[2..].to_owned());
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
        let abs = absolute(w, cwd);
        if is_secret_path(&abs.to_string_lossy()) {
            return false;
        }
        match std::fs::canonicalize(&abs) {
            Ok(r) => !is_secret_path(&r.to_string_lossy()),
            Err(_) => true,
        }
    })
}

/// A plan for `cmd` if it only reads, `None` otherwise. `extra`: the config's
/// `allow:` prefixes (trusted, but still shell-free and secret-checked).
pub fn plan(cmd: &str, extra: &[String], cwd: &str) -> Option<Plan> {
    let toks = tokenize(cmd)?;
    let extra: Vec<Vec<String>> = extra
        .iter()
        .map(|e| e.split_whitespace().map(str::to_owned).collect())
        .collect();
    let mut stages: Vec<Vec<String>> = vec![Vec::new()];
    for t in toks {
        match t {
            Tok::Word(w) => {
                if w.glob {
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
            Tok::Pipe => stages.push(Vec::new()),
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
            if roots.iter().any(|r| root_too_wide(&absolute(r, cwd))) {
                return None;
            }
        }
        out.push(st.argv);
    }
    Some(Plan { stages: out })
}

/// Whether `cmd` only reads (relative to the current directory).
pub fn is_read_only(cmd: &str, extra: &[String]) -> bool {
    plan(cmd, extra, ".").is_some()
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
            "git status --short -b",
            "git log --oneline -n 5",
            "git log -5 --stat",
            "git diff HEAD~1 -- src",
            "git show abc123",
            "git branch",
            "git branch -a",
            "git branch --list 'feat*'",
            "git branch --contains abc",
            "git remote -v",
            "git rev-parse HEAD",
            "git ls-files",
            "git blame -L 1,5 src/main.rs",
            "git --no-pager log",
            "cargo metadata --offline --format-version 1",
            "cargo metadata --frozen",
            "systemctl status sshd",
            "systemctl --user status foo",
            "journalctl -u sshd -n 50 --no-pager",
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
        let p = plan("git diff -- '*.rs'", &[], "/tmp/x").unwrap();
        assert_eq!(p.stages[0].last().unwrap(), "*.rs");
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
    fn review_7_systemctl_and_cargo() {
        for c in [
            "systemctl status -H user@host x",
            "systemctl status --host=h x",
            "systemctl status -M m x",
            "systemctl status --machine=m x",
            "cargo metadata",
            "cargo metadata --no-deps",
            "cargo metadata --config x=y --offline",
        ] {
            assert!(!ok(c), "{c:?}");
        }
        assert!(ok("cargo metadata --offline --no-deps"));
    }

    #[test]
    fn review_8_git_is_neutered() {
        let p = plan("git diff HEAD", &[], "/tmp/x").unwrap();
        assert_eq!(
            p.stages[0],
            [
                "git",
                "--no-pager",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "HEAD"
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
        assert!(ok("git show HEAD:src/main.rs"));
        assert!(ok("grep --file=pats.txt x"));
    }
}
