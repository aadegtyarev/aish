//! Light fish-like highlighting: command resolution, quotes, `$vars`, comments.

/// Style class of a highlighted span; the [`Theme`](crate::Theme) maps it to colors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Class {
    /// Plain text.
    Default,
    /// Unresolvable command word.
    Error,
    /// Quoted string.
    Str,
    /// `$variable`.
    Var,
    /// Comment.
    Comment,
    /// Accent, e.g. the marker of an intercepted line.
    Accent,
}

/// A styled byte range of the buffer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    /// Start byte offset.
    pub start: usize,
    /// End byte offset (exclusive).
    pub end: usize,
    /// Style class.
    pub class: Class,
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
pub(crate) fn highlight(text: &str, res: &dyn Fn(&str) -> bool, out: &mut Vec<Span>) {
    out.clear();
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
                    if plain && !res(word) {
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

    fn res(w: &str) -> bool {
        matches!(
            w,
            "ls" | "git" | "cd" | "echo" | "true" | "if" | "then" | "fi"
        )
    }

    fn hl(t: &str) -> Vec<(String, Class)> {
        let mut v = vec![];
        highlight(t, &res, &mut v);
        v.iter()
            .map(|s| (t[s.start..s.end].to_owned(), s.class))
            .collect()
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
}
