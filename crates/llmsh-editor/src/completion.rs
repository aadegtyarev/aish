//! Completion types and helpers shared by the editor and its host: word
//! splitting, quoting, and the [`Completer`] trait.

/// Upper bound on candidates a source should return.
pub const MAX_ITEMS: usize = 5000;

/// Completion candidates for one word.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Candidates {
    /// Candidates are file names (quote them, mark directories).
    pub filenames: bool,
    /// Do not append a space after a unique match.
    pub nospace: bool,
    /// The candidates (whole words, not suffixes).
    pub items: Vec<String>,
}

/// What is being completed: the words of the current command up to the cursor.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    /// Words of the current command, the last one being completed (raw, with quotes).
    pub words: Vec<String>,
    /// Command text from the segment start to the cursor.
    pub line: String,
    /// Byte offset in the buffer where the current word starts.
    pub word_start: usize,
}

impl Request {
    /// The raw word being completed.
    pub fn cur(&self) -> &str {
        self.words.last().map_or("", String::as_str)
    }
}

fn is_assign(w: &str) -> bool {
    w.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty()
            && !k.starts_with(|c: char| c.is_ascii_digit())
            && k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    })
}

/// Quote-aware split of `text[..cursor]` into the words of the last command.
pub fn parse(text: &str, cursor: usize) -> Request {
    let prefix = &text[..cursor];
    let mut words: Vec<(usize, usize)> = Vec::new(); // (start, end)
    let mut seg_start = 0;
    let mut quote: Option<char> = None;
    let mut cur_start: Option<usize> = None;
    let mut esc = false;
    for (i, c) in prefix.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                esc = true;
            }
            continue;
        }
        match c {
            '\\' => {
                esc = true;
                cur_start.get_or_insert(i);
            }
            '\'' | '"' => {
                quote = Some(c);
                cur_start.get_or_insert(i);
            }
            c if c.is_whitespace() => {
                if let Some(s) = cur_start.take() {
                    words.push((s, i));
                }
            }
            ';' | '|' | '&' | '(' | '`' => {
                if let Some(s) = cur_start.take() {
                    words.push((s, i));
                }
                words.clear();
                seg_start = i + c.len_utf8();
            }
            _ => {
                cur_start.get_or_insert(i);
            }
        }
    }
    let word_start = match cur_start {
        Some(s) => s,
        None => cursor,
    };
    // Leading VAR=value words are not the command.
    let mut start_idx = 0;
    while start_idx < words.len() && is_assign(&prefix[words[start_idx].0..words[start_idx].1]) {
        start_idx += 1;
    }
    let mut out: Vec<String> = words[start_idx..]
        .iter()
        .map(|&(s, e)| prefix[s..e].to_owned())
        .collect();
    out.push(prefix[word_start..].to_owned());
    Request {
        words: out,
        line: prefix[seg_start..].trim_start().to_owned(),
        word_start,
    }
}

/// Undo backslash escapes and a leading quote of a raw word.
pub fn unescape(raw: &str) -> String {
    let body = raw.strip_prefix(['\'', '"']).unwrap_or(raw);
    let mut s = String::with_capacity(body.len());
    let mut it = body.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(n) = it.next() {
                s.push(n);
            }
        } else {
            s.push(c);
        }
    }
    s
}

/// Text to put in the buffer for candidate `cand` replacing the raw word `cur`.
pub fn quote(cand: &str, cur: &str, filenames: bool) -> String {
    if let Some(q) = cur.chars().next().filter(|c| matches!(c, '\'' | '"')) {
        let mut s = String::with_capacity(cand.len() + 1);
        s.push(q);
        s.push_str(cand);
        return s;
    }
    if !filenames {
        return cand.to_owned();
    }
    let mut s = String::with_capacity(cand.len() + 4);
    for (i, c) in cand.char_indices() {
        let special = matches!(
            c,
            ' ' | '\t'
                | '\n'
                | '\\'
                | '\''
                | '"'
                | '`'
                | '$'
                | '&'
                | ';'
                | '('
                | ')'
                | '<'
                | '>'
                | '|'
                | '*'
                | '?'
                | '['
                | ']'
                | '{'
                | '}'
                | '!'
                | '#'
                | '^'
        ) || (c == '~' && i > 0);
        if special {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

/// Longest common prefix (by chars).
pub fn common_prefix(items: &[String]) -> &str {
    let Some(first) = items.first() else {
        return "";
    };
    let mut n = first.len();
    for s in &items[1..] {
        n = first
            .char_indices()
            .zip(s.chars())
            .take_while(|&((_, a), b)| a == b)
            .last()
            .map_or(0, |((i, c), _)| (i + c.len_utf8()).min(n));
    }
    &first[..n.min(first.len())]
}

/// What a [`CompletionSource`] answers right away.
pub enum Outcome {
    /// Candidates are ready now.
    Ready(Candidates),
    /// Slow source: the candidates will be sent through the [`Reply`] later
    /// (from any thread). Stale replies are dropped by the editor.
    Pending,
    /// Nothing to offer.
    None,
}

/// Where a late answer goes. Cloneable and `Send`.
#[derive(Clone)]
pub struct Reply {
    pub(crate) handle: crate::plugin::Handle,
    pub(crate) generation: u64,
}

impl Reply {
    /// Deliver candidates; ignored if the line changed since the request.
    pub fn send(&self, c: Candidates) {
        self.handle.deliver(self.generation, Box::new(c));
    }
}

/// Source of tab-completion candidates for the [`completion_pager`](crate::plugins::CompletionPager)
/// plugin. `complete` runs on the key-handling thread and must return quickly:
/// slow work returns [`Outcome::Pending`] and answers via `reply`.
pub trait CompletionSource: Send {
    /// Candidates for the word being completed.
    fn complete(&mut self, req: &Request, reply: Reply) -> Outcome;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(t: &str) -> Request {
        parse(t, t.len())
    }

    #[test]
    fn split_words() {
        let r = p("git commit -m \"a b\" fo");
        assert_eq!(r.words, ["git", "commit", "-m", "\"a b\"", "fo"]);
        assert_eq!(r.word_start, 20);
        let r = p("ls ");
        assert_eq!(r.words, ["ls", ""]);
        assert_eq!(r.word_start, 3);
        let r = p("cd my\\ dir/su");
        assert_eq!(r.words, ["cd", "my\\ dir/su"]);
        let r = p("echo 'a b");
        assert_eq!(r.words, ["echo", "'a b"]);
    }

    #[test]
    fn segments_and_assignments() {
        let r = p("make && FOO=1 git ch");
        assert_eq!(r.words, ["git", "ch"]);
        assert_eq!(r.line, "FOO=1 git ch");
        assert_eq!(p("a | b ").words, ["b", ""]);
        assert_eq!(p("").words, [""]);
        let r = parse("ls /us -l", 6);
        assert_eq!(r.words, ["ls", "/us"]);
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("my file", "my", true), "my\\ file");
        assert_eq!(quote("a$b", "a", true), "a\\$b");
        assert_eq!(quote("~/x y", "~", true), "~/x\\ y");
        assert_eq!(quote("my file", "\"my", true), "\"my file");
        assert_eq!(quote("--opt=1", "--", false), "--opt=1");
        assert_eq!(unescape("my\\ dir/s"), "my dir/s");
        assert_eq!(unescape("\"a b"), "a b");
    }

    #[test]
    fn lcp() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(common_prefix(&v(&["foobar", "foobaz", "foo"])), "foo");
        assert_eq!(common_prefix(&v(&["中文a", "中文b"])), "中文");
        assert_eq!(common_prefix(&v(&["a", "b"])), "");
        assert_eq!(common_prefix(&v(&["only"])), "only");
    }
}
