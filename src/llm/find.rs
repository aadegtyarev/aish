//! Search of the terminal history: the `find` tool and `aish find`.

use regex::RegexBuilder;

use super::cmd::Ln;
use crate::blocks::Block;

const MAX_HITS: usize = 30;
const LINE_CHARS: usize = 200;

/// One matching line: block number (1 = last), 1-based output line (`None`:
/// the command), text around the match.
#[derive(Debug, PartialEq)]
pub struct Hit {
    pub block: usize,
    pub line: Option<usize>,
    pub text: String,
}

impl Hit {
    fn loc(&self) -> String {
        self.line.map_or("cmd".into(), |l| format!("L{l}"))
    }
}

/// `line` cut to `LINE_CHARS` around char index `at`.
fn snippet(line: &str, at: usize) -> String {
    let line = line.trim().replace('\n', " ");
    let n = line.chars().count();
    if n <= LINE_CHARS {
        return line;
    }
    let from = at.saturating_sub(LINE_CHARS / 4).min(n - LINE_CHARS);
    let s: String = line.chars().skip(from).take(LINE_CHARS).collect();
    let (pre, post) = (
        if from > 0 { "…" } else { "" },
        if from + LINE_CHARS < n { "…" } else { "" },
    );
    format!("{pre}{s}{post}")
}

/// Hits (newest block first, at most `MAX_HITS`) and their total; case-insensitive.
pub fn search<'a>(
    blocks: impl Iterator<Item = &'a Block>,
    query: &str,
    regex: bool,
) -> Result<(Vec<Hit>, usize), String> {
    let pat = if regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    let re = RegexBuilder::new(&pat)
        .case_insensitive(true)
        .build()
        .map_err(|e| format!("bad regex: {e}"))?;
    let (mut hits, mut total) = (Vec::new(), 0);
    for (i, b) in blocks.enumerate() {
        let lines = std::iter::once((None, b.command.as_str()))
            .chain(b.output.lines().enumerate().map(|(k, l)| (Some(k + 1), l)));
        for (line, text) in lines {
            let Some(m) = re.find(text) else { continue };
            total += 1;
            if hits.len() < MAX_HITS {
                let at = text[..m.start()].chars().count();
                hits.push(Hit {
                    block: i + 1,
                    line,
                    text: snippet(text, at),
                });
            }
        }
    }
    Ok((hits, total))
}

/// The `find` tool's result for the model.
pub fn tool_text<'a>(blocks: impl Iterator<Item = &'a Block>, query: &str, regex: bool) -> String {
    match search(blocks, query, regex) {
        Err(e) => format!("error: {e}"),
        Ok((h, _)) if h.is_empty() => "no matches".into(),
        Ok((h, total)) => {
            let mut s: String = h
                .iter()
                .map(|x| format!("block {} {}: {}\n", x.block, x.loc(), x.text))
                .collect();
            if total > h.len() {
                s.push_str(&format!("{} more hits", total - h.len()));
            }
            s.trim_end().into()
        }
    }
}

/// Rows of `aish find <text>`.
pub fn user_lines<'a>(blocks: impl Iterator<Item = &'a Block>, query: &str) -> Vec<Ln> {
    if query.is_empty() {
        return vec![Ln::dim("aish find <text>")];
    }
    match search(blocks, query, false) {
        Ok((h, _)) if h.is_empty() => vec![Ln::dim("no matches")],
        Ok((h, total)) => {
            let mut v: Vec<Ln> = h
                .iter()
                .map(|x| Ln::dim(format!("#{} {}: {}", x.block, x.loc(), x.text)))
                .collect();
            if total > h.len() {
                v.push(Ln::dim(format!("{} more", total - h.len())));
            }
            v
        }
        Err(e) => vec![Ln::dim(e)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blk(cmd: &str, out: &str) -> Block {
        Block {
            command: cmd.into(),
            output: out.into(),
            exit_code: Some(0),
            cwd: "/".into(),
            duration: Default::default(),
        }
    }

    fn two() -> Vec<Block> {
        vec![
            blk("echo Needle", "a\nNEEDLE here\nb"),
            blk("ls", "x\nneedle"),
        ]
    }

    #[test]
    fn plain_case_insensitive_newest_first() {
        let b = two();
        let (h, t) = search(b.iter(), "needle", false).unwrap();
        assert_eq!(t, 3);
        let at: Vec<_> = h.iter().map(|x| (x.block, x.line)).collect();
        assert_eq!(at, [(1, None), (1, Some(2)), (2, Some(2))]);
        assert_eq!(h[0].text, "echo Needle");
    }

    #[test]
    fn plain_is_literal_regex_is_not() {
        let b = [blk("c", "a.c\nabc")];
        assert_eq!(search(b.iter(), "a.c", false).unwrap().1, 1);
        assert_eq!(search(b.iter(), "a.c", true).unwrap().1, 2);
    }

    #[test]
    fn bad_regex_is_an_error() {
        assert!(search(two().iter(), "(", true).is_err());
        assert!(tool_text(two().iter(), "(", true).starts_with("error: bad regex"));
    }

    #[test]
    fn limit_and_total() {
        let b = [blk("c", &"hit\n".repeat(40))];
        let (h, t) = search(b.iter(), "hit", false).unwrap();
        assert_eq!((h.len(), t), (30, 40));
        assert!(tool_text(b.iter(), "hit", false).ends_with("10 more hits"));
        assert_eq!(tool_text(b.iter(), "zzz", false), "no matches");
    }

    #[test]
    fn long_line_keeps_the_match() {
        let line = format!("{}needle{}", "a".repeat(500), "b".repeat(500));
        let b = [blk("c", &line)];
        let h = &search(b.iter(), "needle", false).unwrap().0[0];
        assert!(h.text.contains("needle") && h.text.chars().count() <= LINE_CHARS + 2);
        assert!(h.text.starts_with('…') && h.text.ends_with('…'));
    }

    #[test]
    fn user_rows() {
        let b = two();
        let v = user_lines(b.iter(), "needle");
        assert_eq!(v[0].1, "#1 cmd: echo Needle");
        assert_eq!(v[1].1, "#1 L2: NEEDLE here");
        assert_eq!(user_lines(b.iter(), "")[0].1, "aish find <text>");
    }
}
