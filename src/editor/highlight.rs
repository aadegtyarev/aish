//! Light fish-like highlighting: command resolution, quotes, `$vars`, comments.

use std::collections::HashSet;
use std::ffi::OsString;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    Default,
    /// Unresolvable command word.
    Error,
    Str,
    Var,
    Comment,
    /// The `?` of a model line.
    Accent,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub class: Class,
}

/// `?`, `?N`, `?!` — alone or followed by a space — go to the model; every
/// other `?...` is a shell glob and stays a command.
pub fn is_model_line(s: &str) -> bool {
    let Some(r) = s.strip_prefix('?') else {
        return false;
    };
    let r = if let Some(x) = r.strip_prefix('!') {
        x
    } else {
        r.trim_start_matches(|c: char| c.is_ascii_digit())
    };
    r.is_empty() || r.starts_with(' ')
}

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

/// Knows which words are runnable commands. PATH contents are filled in by a
/// background scan; aliases/functions by the completion sidecar.
#[derive(Default)]
pub struct Resolver {
    path_cmds: HashSet<Box<str>>,
    extra: HashSet<Box<str>>,
    path_env: OsString,
    ready: bool,
}

impl Resolver {
    pub fn scan_path() -> (OsString, HashSet<Box<str>>) {
        let path = std::env::var_os("PATH").unwrap_or_default();
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

    pub fn set_path(&mut self, path: OsString, cmds: HashSet<Box<str>>) {
        self.path_env = path;
        self.path_cmds = cmds;
        self.ready = true;
    }

    pub fn set_extra(&mut self, names: impl IntoIterator<Item = String>) {
        self.extra = names.into_iter().map(String::into_boxed_str).collect();
    }

    /// Has PATH changed since the last scan?
    pub fn path_stale(&self) -> bool {
        self.ready && std::env::var_os("PATH").unwrap_or_default() != self.path_env
    }

    pub fn resolves(&self, w: &str) -> bool {
        if !self.ready
            || BUILTINS.contains(&w)
            || KEYWORDS.contains(&w)
            || self.extra.contains(w)
            || self.path_cmds.contains(w)
        {
            return true;
        }
        if w.contains('/') || w.starts_with('~') {
            let p = std::path::Path::new(w);
            return w.starts_with('~') || p.is_file();
        }
        false
    }
}

fn var_len(s: &str) -> usize {
    // s starts after '$'
    let b = s.as_bytes();
    match b.first() {
        Some(b'{') => s.find('}').map_or(s.len(), |i| i + 1),
        Some(b'(') => 0, // command substitution: leave to the default color
        Some(c) if c.is_ascii_alphabetic() || *c == b'_' => b
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
            .count(),
        Some(b'@' | b'#' | b'?' | b'*' | b'!' | b'$' | b'-' | b'0'..=b'9') => 1,
        _ => 0,
    }
}

/// Fill `out` with non-overlapping, ordered spans for `text`.
pub fn highlight(text: &str, res: &Resolver, out: &mut Vec<Span>) {
    out.clear();
    if is_model_line(text) {
        out.push(Span {
            start: 0,
            end: 1,
            class: Class::Accent,
        });
        return;
    }
    let b = text.as_bytes();
    let mut i = 0;
    let mut cmd_pos = true;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' => i += 1,
            b';' | b'|' | b'&' | b'(' | b')' => {
                cmd_pos = c != b')';
                i += 1;
            }
            b'#' => {
                out.push(Span {
                    start: i,
                    end: b.len(),
                    class: Class::Comment,
                });
                return;
            }
            _ => {
                // A word: up to unquoted whitespace or operator.
                let start = i;
                let mut plain = true;
                while i < b.len() {
                    match b[i] {
                        b' ' | b'\t' | b'\n' | b';' | b'|' | b'&' | b'(' | b')' => break,
                        q @ (b'\'' | b'"') => {
                            plain = false;
                            let s = i;
                            i += 1;
                            while i < b.len() && b[i] != q {
                                if q == b'"' && b[i] == b'\\' {
                                    i += 1;
                                }
                                i += 1;
                            }
                            i = (i + 1).min(b.len());
                            push(out, s, i, Class::Str);
                        }
                        b'$' => {
                            plain = false;
                            let n = var_len(&text[i + 1..]);
                            if n > 0 {
                                push(out, i, i + 1 + n, Class::Var);
                                i += 1 + n;
                            } else {
                                i += 1;
                            }
                        }
                        b'\\' => {
                            plain = false;
                            i += 2;
                        }
                        _ => i += 1,
                    }
                }
                i = i.min(b.len());
                let word = &text[start..i];
                let assign = word.split_once('=').is_some_and(|(k, _)| {
                    !k.is_empty() && k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                });
                if cmd_pos && !assign {
                    if plain && !res.resolves(word) {
                        push(out, start, i, Class::Error);
                    }
                    cmd_pos = plain
                        && matches!(
                            word,
                            "if" | "then"
                                | "do"
                                | "else"
                                | "elif"
                                | "while"
                                | "until"
                                | "!"
                                | "time"
                                | "{"
                                | "sudo"
                                | "exec"
                                | "command"
                                | "builtin"
                        );
                }
            }
        }
    }
}

/// Keep spans ordered: parts of a word already covered are not duplicated.
fn push(out: &mut Vec<Span>, start: usize, end: usize, class: Class) {
    if out.last().is_none_or(|l| l.end <= start) {
        out.push(Span { start, end, class });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res() -> Resolver {
        let mut r = Resolver::default();
        let mut s = HashSet::new();
        s.insert("ls".into());
        s.insert("git".into());
        r.set_path("x".into(), s);
        r
    }

    fn hl(t: &str) -> Vec<(String, Class)> {
        let mut v = vec![];
        highlight(t, &res(), &mut v);
        v.iter()
            .map(|s| (t[s.start..s.end].to_owned(), s.class))
            .collect()
    }

    #[test]
    fn model_lines() {
        for t in ["?", "? x", "?3 x", "?!", "?! ", "?12", "? "] {
            assert!(is_model_line(t), "{t}");
        }
        for t in ["?.txt", "??", "?x", "?3x", "?!x", "", "ls ?", " ?", "\\?"] {
            assert!(!is_model_line(t), "{t}");
        }
    }

    #[test]
    fn first_word_resolution() {
        assert_eq!(hl("ls -l"), vec![]);
        assert_eq!(hl("lss -l"), vec![("lss".into(), Class::Error)]);
        assert_eq!(hl("cd x"), vec![]);
        assert_eq!(
            hl("if true; then nope; fi"),
            vec![("nope".into(), Class::Error)]
        );
        assert_eq!(hl("ls | grepp x"), vec![("grepp".into(), Class::Error)]);
        assert_eq!(hl("FOO=1 ls"), vec![]);
    }

    #[test]
    fn quotes_vars_comments() {
        assert_eq!(
            hl("git commit -m \"a b\" 'c' $HOME ${X}y # note"),
            vec![
                ("\"a b\"".into(), Class::Str),
                ("'c'".into(), Class::Str),
                ("$HOME".into(), Class::Var),
                ("${X}".into(), Class::Var),
                ("# note".into(), Class::Comment),
            ]
        );
        assert_eq!(
            hl("ls 'unterminated"),
            vec![("'unterminated".into(), Class::Str)]
        );
        assert_eq!(hl("echo a#b"), vec![]);
    }

    #[test]
    fn model_line_accent() {
        assert_eq!(hl("? why is $x"), vec![("?".into(), Class::Accent)]);
        // glob stays a shell line
        assert_eq!(hl("?.txt"), vec![("?.txt".into(), Class::Error)]);
    }
}
