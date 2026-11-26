//! Secret masking for text that leaves the machine.

const SECRET_WORDS: &[&str] = &["TOKEN", "SECRET", "PASSWORD", "PASSWD"];

fn secret_name(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    n.ends_with("_KEY")
        || n.ends_with("-KEY")
        || n == "APIKEY"
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
    // Authorization header: mask everything after the colon.
    if let Some(p) = find_ci(line, "authorization:") {
        out.push_str(&mask_urls(&line[..p + 14]));
        out.push_str(" ***");
        return;
    }
    let line = mask_urls(line);
    // NAME=value with a secret-looking name.
    let mut last = 0;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'=' {
            i += 1;
            continue;
        }
        let ns = line[..i]
            .char_indices()
            .rev()
            .take_while(|&(_, c)| is_name_char(c))
            .last()
            .map_or(i, |(p, _)| p);
        let name = &line[ns..i];
        if ns >= last && !name.is_empty() && secret_name(name) {
            let vs = i + 1;
            let ve = value_end(&line[vs..]) + vs;
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
}

fn value_end(v: &str) -> usize {
    match v.chars().next() {
        Some(q @ ('"' | '\'')) => v[1..].find(q).map_or(v.len(), |e| e + 2),
        _ => v.find(char::is_whitespace).unwrap_or(v.len()),
    }
}

fn find_ci(h: &str, needle: &str) -> Option<usize> {
    h.to_ascii_lowercase().find(needle)
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
}
