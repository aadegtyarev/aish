//! Secret masking for text that leaves the machine.

const SECRET_WORDS: &[&str] = &["TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"];
/// Headers whose whole value is a secret.
const SECRET_HEADERS: &[&str] = &["authorization:", "cookie:", "proxy-authorization:"];

fn secret_name(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    matches!(
        n.as_str(),
        "KEY" | "APIKEY" | "PASS" | "SIG" | "SIGNATURE" | "AUTH"
    ) || n.ends_with("_KEY")
        || n.ends_with("-KEY")
        || n.ends_with(".KEY")
        || n.ends_with("_SIG")
        || SECRET_WORDS.iter().any(|w| n.contains(w))
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'
}

/// Replace secrets in `text`; the result has the same line structure.
pub fn mask(text: &str) -> String {
    let text = mask_private_keys(text);
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        mask_line(line, &mut out);
    }
    out
}

fn mask_private_keys(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(b) = rest.find("-----BEGIN ") {
        let after = &rest[b..];
        let head_end = after[11..].find("-----").map(|e| e + 16);
        let is_priv = head_end.is_some_and(|e| after[..e].contains("PRIVATE KEY"));
        if !is_priv {
            out.push_str(&rest[..b + 11]);
            rest = &rest[b + 11..];
            continue;
        }
        out.push_str(&rest[..b]);
        out.push_str("[masked private key]");
        match after.find("-----END ") {
            Some(e) => match after[e + 9..].find("-----") {
                Some(t) => rest = &after[e + 9 + t + 5..],
                None => rest = "",
            },
            None => rest = "",
        }
    }
    out.push_str(rest);
    out
}

fn mask_line(line: &str, out: &mut String) {
    let lower = line.to_ascii_lowercase();
    for h in SECRET_HEADERS {
        if let Some(p) = lower.find(h) {
            out.push_str(&mask_line_inner(&line[..p + h.len()]));
            out.push_str(" ***");
            return;
        }
    }
    out.push_str(&mask_line_inner(line));
}

fn mask_line_inner(line: &str) -> String {
    let l = mask_urls(line);
    let l = mask_flags(&l);
    let l = mask_assign(&l);
    mask_shapes(&l)
}

/// `name = value`, `name: value`, `"name": "value"` with a secret-looking name.
fn mask_assign(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut last = 0;
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'=' && b[i] != b':' {
            i += 1;
            continue;
        }
        let mut j = i;
        while j > 0 && b[j - 1] == b' ' {
            j -= 1;
        }
        if j > 0 && matches!(b[j - 1], b'"' | b'\'') {
            j -= 1;
        }
        let ns = line[..j]
            .char_indices()
            .rev()
            .take_while(|&(_, c)| is_name_char(c))
            .last()
            .map_or(j, |(p, _)| p);
        let name = &line[ns..j];
        if ns >= last && !name.is_empty() && secret_name(name) {
            let mut vs = i + 1;
            while vs < b.len() && b[vs] == b' ' {
                vs += 1;
            }
            let ve = vs + value_end(&line[vs..], b[i] == b':');
            if ve > vs {
                out.push_str(&line[last..vs]);
                out.push_str("***");
                last = ve;
                i = ve;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&line[last..]);
    out
}

fn value_end(v: &str, colon: bool) -> usize {
    match v.chars().next() {
        Some(q @ ('"' | '\'')) => v[1..].find(q).map_or(v.len(), |e| e + 2),
        _ => v
            .find(|c: char| {
                c.is_whitespace() || c == '&' || c == '"' || (colon && matches!(c, ',' | '}' | ';'))
            })
            .unwrap_or(v.len()),
    }
}

/// `--password x`, `-u user:pass` (curl), `-p x` / `-pX` (mysql, sshpass).
fn mask_flags(line: &str) -> String {
    let words: Vec<&str> = line.split(' ').collect();
    let first = words.iter().find(|w| !w.is_empty()).copied().unwrap_or("");
    let first = first.rsplit('/').next().unwrap_or(first);
    let pass_cmd = matches!(first, "mysql" | "mysqldump" | "mariadb" | "sshpass");
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut mask_next: Option<bool> = None; // Some(true): whole word, Some(false): after ':'
    for w in words {
        if let Some(whole) = mask_next.take() {
            if !w.is_empty() {
                out.push(match (whole, w.split_once(':')) {
                    (false, Some((u, _))) => format!("{u}:***"),
                    (false, None) => w.to_owned(),
                    _ => "***".into(),
                });
                continue;
            }
            mask_next = Some(whole);
        }
        let name = w.trim_start_matches('-');
        let long = w.starts_with("--");
        if long && !w.contains('=') && secret_name(name) {
            mask_next = Some(true);
        } else if first == "curl" && matches!(w, "-u" | "--user" | "--proxy-user") {
            mask_next = Some(false);
        } else if pass_cmd && w == "-p" {
            mask_next = Some(true);
        } else if pass_cmd && w.starts_with("-p") && w.len() > 2 && !long {
            out.push("-p***".into());
            continue;
        }
        out.push(w.to_owned());
    }
    out.join(" ")
}

fn token_shape(w: &str) -> bool {
    let n = w.len();
    let alnum = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    if w.starts_with("sk-") && n >= 20 && alnum(w) {
        return true;
    }
    for p in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
        if w.starts_with(p) && n >= 30 && alnum(w) {
            return true;
        }
    }
    if w.starts_with("github_pat_") && n >= 30 && alnum(w) {
        return true;
    }
    if (w.starts_with("AKIA") || w.starts_with("ASIA"))
        && n == 20
        && w.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return true;
    }
    if w.starts_with("glpat-") && n >= 20 && alnum(w) {
        return true;
    }
    if w.len() > 10
        && w.starts_with("xox")
        && matches!(w.as_bytes()[3], b'a' | b'b' | b'p' | b'r')
        && w.as_bytes()[4] == b'-'
    {
        return true;
    }
    let parts: Vec<&str> = w.split('.').collect();
    parts.len() == 3
        && w.starts_with("eyJ")
        && parts[1].starts_with("eyJ")
        && parts.iter().all(|p| !p.is_empty() && alnum(p))
}

/// Replace bare tokens by their shape (`sk-...`, GitHub, AWS, Slack, GitLab, JWT).
fn mask_shapes(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut start = None;
    let flush = |out: &mut String, s: &str| {
        if token_shape(s) {
            out.push_str("***");
        } else {
            out.push_str(s);
        }
    };
    for (i, c) in line.char_indices() {
        if is_name_char(c) {
            start.get_or_insert(i);
        } else {
            if let Some(s) = start.take() {
                flush(&mut out, &line[s..i]);
            }
            out.push(c);
        }
    }
    if let Some(s) = start {
        flush(&mut out, &line[s..]);
    }
    out
}

/// `scheme://user:pass@host` -> `scheme://user:***@host`.
fn mask_urls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(p) = rest.find("://") {
        let (head, tail) = rest.split_at(p + 3);
        out.push_str(head);
        let auth_end = tail
            .find(|c: char| c == '/' || c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(tail.len());
        let auth = &tail[..auth_end];
        match auth.rfind('@') {
            Some(at) => match auth[..at].split_once(':') {
                Some((user, _)) => {
                    out.push_str(user);
                    out.push_str(":***");
                    rest = &tail[at..];
                }
                None => rest = tail,
            },
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_assignments() {
        assert_eq!(
            mask("export OPENAI_API_KEY=sk-abc123 ls"),
            "export OPENAI_API_KEY=*** ls"
        );
        assert_eq!(mask("GITHUB_TOKEN=\"a b c\" x"), "GITHUB_TOKEN=*** x");
        assert_eq!(mask("DB_PASSWORD=hunter2"), "DB_PASSWORD=***");
        assert_eq!(
            mask("mysql --password=hunter2 -u me"),
            "mysql --password=*** -u me"
        );
        assert_eq!(mask("MY_SECRET_THING=zzz"), "MY_SECRET_THING=***");
        // not secrets
        assert_eq!(mask("KEYBOARD=us PATH=/bin"), "KEYBOARD=us PATH=/bin");
        assert_eq!(mask("a=b"), "a=b");
    }

    #[test]
    fn authorization_header() {
        assert_eq!(
            mask("curl -H 'Authorization: Bearer abc' x"),
            "curl -H 'Authorization: ***"
        );
        assert_eq!(mask("authorization: Basic zzz"), "authorization: ***");
    }

    #[test]
    fn private_key_block() {
        let t = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIE\nabc\n-----END RSA PRIVATE KEY-----\nafter";
        assert_eq!(mask(t), "before\n[masked private key]\nafter");
        assert_eq!(
            mask("-----BEGIN PRIVATE KEY-----\nxx"),
            "[masked private key]"
        );
        assert_eq!(
            mask("-----BEGIN CERTIFICATE-----\nxx"),
            "-----BEGIN CERTIFICATE-----\nxx"
        );
    }

    #[test]
    fn url_credentials() {
        assert_eq!(
            mask("git clone https://bob:s3cret@github.com/x/y.git"),
            "git clone https://bob:***@github.com/x/y.git"
        );
        assert_eq!(mask("https://github.com/x@y"), "https://github.com/x@y");
        assert_eq!(mask("ssh://user@host/p"), "ssh://user@host/p");
    }

    #[test]
    fn config_file_and_json_forms() {
        assert_eq!(mask("api_key = abc123"), "api_key = ***");
        assert_eq!(mask("password: hunter2"), "password: ***");
        assert_eq!(
            mask(r#"{"api_key": "abc", "x": 1}"#),
            r#"{"api_key": ***, "x": 1}"#
        );
        assert_eq!(mask("secret: 'a b'"), "secret: ***");
        assert_eq!(mask("name: bob"), "name: bob");
    }

    #[test]
    fn flags() {
        assert_eq!(
            mask("mysql -u root -p hunter2 db"),
            "mysql -u root -p *** db"
        );
        assert_eq!(mask("mysql -phunter2 db"), "mysql -p*** db");
        assert_eq!(mask("sshpass -p pw ssh x"), "sshpass -p *** ssh x");
        assert_eq!(
            mask("tool --password hunter2 run"),
            "tool --password *** run"
        );
        assert_eq!(
            mask("curl -u bob:pw https://x/y"),
            "curl -u bob:*** https://x/y"
        );
        assert_eq!(mask("mkdir -p a/b"), "mkdir -p a/b");
    }

    #[test]
    fn bare_tokens() {
        let sk = "sk-proj-abcdefghijklmnopqrstuvwx";
        assert_eq!(mask(&format!("echo {sk} done")), "echo *** done");
        assert_eq!(mask("ghp_abcdefghijklmnopqrstuvwxyz0123456789"), "***");
        assert_eq!(mask("github_pat_11ABCDEFG0123456789_abcdefghij"), "***");
        assert_eq!(mask("id AKIAIOSFODNN7EXAMPLE x"), "id *** x");
        assert_eq!(mask("glpat-abcdefghijklmnopqrstu"), "***");
        assert_eq!(mask("xoxb-123456789-abcdef"), "***");
        assert_eq!(
            mask("t=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"),
            "t=***"
        );
        assert_eq!(mask("ls -la /usr/lib/skeleton"), "ls -la /usr/lib/skeleton");
    }

    #[test]
    fn url_query_secrets() {
        assert_eq!(
            mask("curl 'https://h/p?id=1&token=abc&sig=zz&x=2'"),
            "curl 'https://h/p?id=1&token=***&sig=***&x=2'"
        );
        assert_eq!(mask("https://h/?key=abc"), "https://h/?key=***");
    }
}
