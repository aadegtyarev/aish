//! The read-only allowlist: which commands the agent may run by itself.
//!
//! The command line is tokenized like a shell would (quotes, escapes) and
//! every simple command must be known harmless. Anything the tokenizer does
//! not understand, or that could write, spawn, substitute or background,
//! is rejected: the caller then hands the command to the user instead.

/// A piece of a command line.
#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Pipe,
    And,
    Or,
}

/// Tokenize a command line. `None` = something we refuse to reason about
/// (substitution, redirection to a file, `;`, `&`, newline, subshell, ...).
fn tokenize(s: &str) -> Option<Vec<Tok>> {
    let cs: Vec<char> = s.chars().collect();
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut i = 0;
    // The word just before a `>` may be an fd number (`2>&1`): dropped.
    let flush = |cur: &mut String, in_word: &mut bool, toks: &mut Vec<Tok>| {
        if *in_word {
            toks.push(Tok::Word(std::mem::take(cur)));
            *in_word = false;
        }
    };
    while i < cs.len() {
        let c = cs[i];
        match c {
            '\'' => {
                in_word = true;
                i += 1;
                loop {
                    match cs.get(i) {
                        None => return None,
                        Some('\'') => break,
                        Some(&x) => cur.push(x),
                    }
                    i += 1;
                }
            }
            '"' => {
                in_word = true;
                i += 1;
                loop {
                    match cs.get(i) {
                        None => return None,
                        Some('"') => break,
                        Some('`') => return None,
                        Some('$') if matches!(cs.get(i + 1), Some('(' | '{')) => return None,
                        Some('\\') => {
                            i += 1;
                            match cs.get(i) {
                                None => return None,
                                Some(&x @ ('"' | '\\' | '$' | '`')) => cur.push(x),
                                Some(&x) => {
                                    cur.push('\\');
                                    cur.push(x);
                                }
                            }
                        }
                        Some(&x) => cur.push(x),
                    }
                    i += 1;
                }
            }
            '\\' => {
                in_word = true;
                i += 1;
                match cs.get(i) {
                    None | Some('\n') => return None,
                    Some(&x) => cur.push(x),
                }
            }
            '`' => return None,
            '$' if matches!(cs.get(i + 1), Some('(' | '{')) => return None,
            ' ' | '\t' => flush(&mut cur, &mut in_word, &mut toks),
            '\n' | ';' | '(' | ')' => return None,
            '|' => {
                flush(&mut cur, &mut in_word, &mut toks);
                if cs.get(i + 1) == Some(&'|') {
                    toks.push(Tok::Or);
                    i += 1;
                } else if cs.get(i + 1) == Some(&'&') {
                    return None; // |& pipes stderr: not worth a case
                } else {
                    toks.push(Tok::Pipe);
                }
            }
            '&' => {
                flush(&mut cur, &mut in_word, &mut toks);
                if cs.get(i + 1) == Some(&'&') {
                    toks.push(Tok::And);
                    i += 1;
                } else {
                    return None; // background / `&>`
                }
            }
            '>' | '<' => {
                // Only `>/dev/null`, `>>/dev/null` and `N>&M` are fine.
                if c == '<' {
                    return None;
                }
                if in_word && cur.chars().all(|d| d.is_ascii_digit()) {
                    cur.clear();
                    in_word = false; // the fd number
                } else {
                    flush(&mut cur, &mut in_word, &mut toks);
                }
                i += 1;
                if cs.get(i) == Some(&'>') {
                    i += 1;
                }
                if cs.get(i) == Some(&'&') {
                    i += 1;
                    let d = cs.get(i).copied()?;
                    if !(d.is_ascii_digit() || d == '-')
                        || cs.get(i + 1).is_some_and(|x| !x.is_whitespace())
                    {
                        return None;
                    }
                    i += 1;
                    continue;
                }
                while matches!(cs.get(i), Some(' ' | '\t')) {
                    i += 1;
                }
                let rest: String = cs[i..].iter().take_while(|x| !x.is_whitespace()).collect();
                if rest != "/dev/null" {
                    return None;
                }
                i += rest.chars().count();
                continue;
            }
            _ => {
                in_word = true;
                cur.push(c);
            }
        }
        i += 1;
    }
    flush(&mut cur, &mut in_word, &mut toks);
    Some(toks)
}

/// Files whose content must never go to a model by our own hand.
pub fn is_secret_path(p: &str) -> bool {
    let home = std::env::var("HOME").unwrap_or_default();
    let expanded = match p.strip_prefix("~/") {
        Some(r) if !home.is_empty() => format!("{home}/{r}"),
        _ => p.to_owned(),
    };
    let parts: Vec<&str> = expanded.split('/').filter(|x| !x.is_empty()).collect();
    let Some(base) = parts.last() else {
        return false;
    };
    let l = base.to_ascii_lowercase();
    parts.contains(&".ssh")
        || parts.contains(&".gnupg")
        || l.ends_with(".pem")
        || l.starts_with(".env")
        || l == ".netrc"
        || l == "_netrc"
        || (parts.len() >= 2
            && parts[parts.len() - 2] == ".aws"
            && (l == "credentials" || l == "config"))
        || expanded.ends_with(".config/llmsh/config.yaml")
}

const SIMPLE: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "stat", "file", "du", "df", "free", "uname", "whoami", "id",
    "ps", "which", "type", "pwd", "date", "grep", "rg", "find",
];

const GIT_SUBS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "rev-parse",
    "ls-files",
    "blame",
];

fn short_cluster_has(a: &str, set: &[char]) -> bool {
    a.starts_with('-') && !a.starts_with("--") && a[1..].chars().any(|c| set.contains(&c))
}

fn starts_any(a: &str, pre: &[&str]) -> bool {
    pre.iter()
        .any(|p| a == *p || a.starts_with(&format!("{p}=")))
}

fn git_ok(args: &[String]) -> bool {
    let mut a = args.iter().map(String::as_str).peekable();
    while a.peek() == Some(&"--no-pager") {
        a.next();
    }
    let Some(sub) = a.next() else { return false };
    let rest: Vec<&str> = a.collect();
    match sub {
        s if GIT_SUBS.contains(&s) => !rest
            .iter()
            .any(|x| starts_any(x, &["--output", "--ext-diff", "--open-files-in-pager"])),
        "branch" => {
            let bad = [
                "--delete",
                "--move",
                "--copy",
                "--force",
                "--set-upstream-to",
                "--unset-upstream",
                "--edit-description",
                "--track",
                "--no-track",
            ];
            if rest.iter().any(|x| {
                starts_any(x, &bad)
                    || short_cluster_has(x, &['d', 'D', 'm', 'M', 'c', 'C', 'f', 'u'])
            }) {
                return false;
            }
            let lists = rest.iter().any(|x| {
                matches!(
                    *x,
                    "-l" | "--list" | "--contains" | "--merged" | "--no-merged" | "--points-at"
                )
            });
            lists || rest.iter().all(|x| x.starts_with('-'))
        }
        "remote" => rest.is_empty() || rest == ["-v"] || rest == ["--verbose"],
        _ => false,
    }
}

fn simple_ok(name: &str, args: &[String]) -> bool {
    let a = || args.iter().map(String::as_str);
    match name {
        "tail" => !a()
            .any(|x| short_cluster_has(x, &['f', 'F']) || starts_any(x, &["--follow", "--retry"])),
        "find" => !a().any(|x| {
            matches!(
                x,
                "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-fls"
            ) || x.starts_with("-fprint")
        }),
        "rg" => !a().any(|x| starts_any(x, &["--pre", "--hostname-bin", "--search-zip", "-z"])),
        "date" => !a().any(|x| short_cluster_has(x, &['s']) || starts_any(x, &["--set"])),
        "file" => !a().any(|x| short_cluster_has(x, &['C']) || x == "--compile"),
        _ => true,
    }
}

fn journalctl_ok(args: &[String]) -> bool {
    !args.iter().any(|x| {
        short_cluster_has(x, &['f'])
            || starts_any(
                x,
                &[
                    "--follow",
                    "--rotate",
                    "--flush",
                    "--sync",
                    "--verify",
                    "--setup-keys",
                    "--update-catalog",
                    "--relinquish-var",
                    "--smart-relinquish-var",
                ],
            )
            || x.starts_with("--vacuum")
    })
}

fn is_assignment(w: &str) -> bool {
    let Some((k, _)) = w.split_once('=') else {
        return false;
    };
    !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn command_ok(words: &[String], extra: &[Vec<String>]) -> bool {
    let Some(name) = words.first() else {
        return false;
    };
    if words.iter().any(|w| is_secret_path(w)) {
        return false;
    }
    if extra
        .iter()
        .any(|p| !p.is_empty() && words.len() >= p.len() && words[..p.len()] == p[..])
    {
        return true;
    }
    if is_assignment(name) || name.contains(['$', '/', '*', '?', '[']) {
        return false;
    }
    let args = &words[1..];
    if SIMPLE.contains(&name.as_str()) {
        return simple_ok(name, args);
    }
    match name.as_str() {
        "git" => git_ok(args),
        "cargo" => args.first().is_some_and(|a| a == "metadata"),
        "systemctl" => {
            let mut a = args.iter().map(String::as_str).peekable();
            while matches!(a.peek(), Some(&"--user" | &"--system" | &"--no-pager")) {
                a.next();
            }
            a.next() == Some("status")
        }
        "journalctl" => journalctl_ok(args),
        _ => false,
    }
}

/// Whether `cmd` only reads: every simple command in the `|`/`&&`/`||`
/// chain is on the allowlist (built-in, or a prefix from `extra`, the
/// config's `allow:` entries).
pub fn is_read_only(cmd: &str, extra: &[String]) -> bool {
    let extra: Vec<Vec<String>> = extra
        .iter()
        .map(|e| e.split_whitespace().map(str::to_owned).collect())
        .collect();
    let Some(toks) = tokenize(cmd) else {
        return false;
    };
    let mut seg: Vec<String> = Vec::new();
    for t in toks {
        match t {
            Tok::Word(w) => seg.push(w),
            Tok::Pipe | Tok::And | Tok::Or => {
                if !command_ok(&seg, &extra) {
                    return false;
                }
                seg.clear();
            }
        }
    }
    command_ok(&seg, &extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(c: &str) -> bool {
        is_read_only(c, &[])
    }

    #[test]
    fn allows_plain_reads() {
        for c in [
            "ls",
            "ls -la /tmp",
            "cat Cargo.toml",
            "head -n 5 src/main.rs",
            "tail -n 20 log.txt",
            "wc -l *.rs",
            "grep -rn 'foo bar' src",
            "rg \"TODO\" -g '*.rs'",
            "find . -name '*.rs' -type f",
            "stat file",
            "du -sh .",
            "df -h",
            "free -m",
            "uname -a",
            "whoami",
            "id",
            "ps aux",
            "which cargo",
            "type ls",
            "pwd",
            "date +%s",
            "git status",
            "git status --short",
            "git log --oneline -n 5",
            "git diff HEAD~1",
            "git show abc123",
            "git branch",
            "git branch -a",
            "git branch --list 'feat*'",
            "git branch --contains abc",
            "git remote -v",
            "git rev-parse HEAD",
            "git ls-files",
            "git blame src/main.rs",
            "git --no-pager log",
            "cargo metadata --format-version 1",
            "systemctl status sshd",
            "systemctl --user status foo",
            "journalctl -u sshd -n 50",
            "journalctl -n50 --no-pager",
            "ls | head -3",
            "git status && git log -1",
            "ls || pwd",
            "ls 2>&1 | grep x",
            "ls >/dev/null",
            "cat f 2>/dev/null",
            "cat \"a b\"",
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
            "cat < in",
            "cat <<EOF",
            "cat <(ls)",
            "ls $(rm x)",
            "ls `rm x`",
            "ls \"$(rm x)\"",
            "ls \"`rm x`\"",
            "ls ${HOME:=x}",
            "FOO=1 ls",
            "ls | sh",
            "ls | xargs rm",
            "ls && rm x",
            "ls || rm x",
            "ls | tee f",
            "tail -f log",
            "tail -F log",
            "tail -fn5 log",
            "tail --follow log",
            "tail --follow=name log",
            "find . -exec rm {} ;",
            "find . -execdir rm {} +",
            "find . -delete",
            "find . -ok rm {} ;",
            "find . -fprint out",
            "find . -fprintf out x",
            "git push",
            "git commit -m x",
            "git checkout x",
            "git reset --hard",
            "git branch -d x",
            "git branch -D x",
            "git branch -vd x",
            "git branch -m a b",
            "git branch newname",
            "git branch --delete x",
            "git remote add x y",
            "git remote remove x",
            "git -c core.pager=sh log",
            "git diff --output=f",
            "git",
            "cargo build",
            "cargo run",
            "systemctl restart sshd",
            "systemctl --user stop x",
            "journalctl --vacuum-size=1M",
            "journalctl --rotate",
            "journalctl -f",
            "journalctl -fu x",
            "journalctl --follow",
            "rg --pre 'sh' x",
            "date -s 'now'",
            "sudo ls",
            "/bin/ls",
            "./ls",
            "$CMD",
            "ls (",
            "( ls )",
            "ls |& cat",
            "ls 2>file",
            "ls > /dev/null.txt",
            "ls '",
            "ls \"",
            "cat ~/.ssh/id_rsa",
            "cat server.pem",
            "cat .env",
            "cat .env.local",
            "cat ~/.aws/credentials",
            "cat ~/.netrc",
            "cat ~/.config/llmsh/config.yaml",
            "ls | ",
            "| ls",
            "ls &&",
        ] {
            assert!(!ok(c), "{c:?}");
        }
    }

    #[test]
    fn quotes_hide_operators() {
        assert!(ok("grep 'a;b' f"));
        assert!(ok("grep \"a && b\" f"));
        assert!(ok("grep a\\;b f"));
        assert!(!ok("grep a\\;b f; ls"));
        // single quotes do not hide a real substitution outside them
        assert!(!ok("ls '$(x)'$(x)"));
        assert!(ok("grep '$(x)' f"));
    }

    #[test]
    fn config_prefixes_extend_the_list() {
        let extra = vec!["make check".to_owned(), "docker ps".to_owned()];
        assert!(is_read_only("make check -j4", &extra));
        assert!(is_read_only("docker ps -a | head", &extra));
        assert!(!is_read_only("make install", &extra));
        assert!(!is_read_only("make check; rm x", &extra));
        assert!(!is_read_only("docker rm x", &extra));
    }

    #[test]
    fn secret_paths() {
        for p in [
            "~/.ssh/id_rsa",
            "/home/u/.ssh/config",
            "key.PEM",
            ".env",
            "a/.env.prod",
            "/root/.aws/credentials",
            "~/.netrc",
            "/h/.config/llmsh/config.yaml",
        ] {
            assert!(is_secret_path(p), "{p}");
        }
        for p in ["src/main.rs", "environment.txt", "/etc/hosts", "a.pem.txt"] {
            assert!(!is_secret_path(p), "{p}");
        }
    }
}
