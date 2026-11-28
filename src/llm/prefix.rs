//! The question prefix (`?` by default) and the forms a line may take with it.

/// Characters with no meaning at the start of a bash line. See docs/UX.md.
pub const ALLOWED: &[char] = &['?', ',', '@', '#', ';'];

/// `? , @ # ;` for messages.
pub fn allowed_list() -> String {
    ALLOWED
        .iter()
        .map(char::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The config value as a prefix character.
pub fn from_config(s: &str) -> Result<char, String> {
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) if ALLOWED.contains(&c) => Ok(c),
        _ => Err(format!(
            "prefix must be one of: {} (got '{s}')",
            allowed_list()
        )),
    }
}

/// A line that starts with the prefix, understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Form {
    /// `?`, `? text`, `?N`, `?N text`.
    Question {
        blocks: usize,
        text: String,
        /// Legacy `?! text`: forget the conversation first.
        reset: bool,
    },
    /// `?n` (and the hidden legacy `?!`).
    New,
    /// `?c`
    Compact,
    /// `?h`
    Help,
}

/// What `line` is for prefix `p`; `None` = an ordinary shell line (`?.txt`,
/// `??`, `,foo`, `#comment`, ...).
pub fn parse(line: &str, p: char) -> Option<Form> {
    // a paste of several lines is always for the shell
    if line.contains(['\n', '\r']) {
        return None;
    }
    // (trailing blanks never turn `?n` into something else)
    let r = line.strip_prefix(p)?.trim_end();
    let q = |blocks: usize, text: &str, reset: bool| Form::Question {
        blocks,
        text: text.trim().to_owned(),
        reset,
    };
    if r.is_empty() || r.starts_with(' ') {
        return Some(q(1, r, false));
    }
    match r {
        "n" => return Some(Form::New),
        "c" => return Some(Form::Compact),
        "h" => return Some(Form::Help),
        _ => {}
    }
    if p == '?' {
        // hidden alias of `?n`, kept for one release
        if r == "!" {
            return Some(Form::New);
        }
        if let Some(t) = r.strip_prefix("! ") {
            return Some(if t.trim().is_empty() {
                Form::New
            } else {
                q(1, t, true)
            });
        }
    }
    let digits = r.chars().take_while(char::is_ascii_digit).count();
    let (n, rest) = r.split_at(digits);
    if digits > 0 && (rest.is_empty() || rest.starts_with(' ')) {
        let blocks = n.parse().unwrap_or(20usize).clamp(1, 20);
        return Some(q(blocks, rest, false));
    }
    None
}

/// Is `line` ours (a question or a conversation command)?
pub fn is_model_line(line: &str, p: char) -> bool {
    parse(line, p).is_some()
}

/// Forms for the help (`?h`, `ai help`): head, description.
pub fn forms(p: char) -> Vec<(String, &'static str)> {
    vec![
        (format!("{p} text"), "ask"),
        (format!("{p}"), "about the last output"),
        (format!("{p}N"), "about the last N blocks"),
        (format!("{p}n"), "new conversation"),
        (format!("{p}c"), "compact conversation"),
        (format!("{p}h"), "this help"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(blocks: usize, text: &str) -> Option<Form> {
        Some(Form::Question {
            blocks,
            text: text.into(),
            reset: false,
        })
    }

    #[test]
    fn every_allowed_prefix_asks() {
        for &p in ALLOWED {
            assert_eq!(parse(&format!("{p}"), p), q(1, ""), "{p}");
            assert_eq!(parse(&format!("{p} why"), p), q(1, "why"), "{p}");
            assert_eq!(parse(&format!("{p}3"), p), q(3, ""), "{p}");
            assert_eq!(parse(&format!("{p}3 why  so"), p), q(3, "why  so"), "{p}");
            assert_eq!(parse(&format!("{p}n"), p), Some(Form::New), "{p}");
            assert_eq!(parse(&format!("{p}c"), p), Some(Form::Compact), "{p}");
            assert_eq!(parse(&format!("{p}h"), p), Some(Form::Help), "{p}");
        }
    }

    #[test]
    fn everything_else_is_a_shell_line() {
        for &p in ALLOWED {
            for t in [
                format!("{p}.txt"),
                format!("{p}{p}"),
                format!("{p}foo"),
                format!("{p}3x"),
                format!("{p}nn"),
                format!("{p}n x"),
                format!("{p}!x"),
                format!("ls {p}"),
                format!(" {p}"),
                format!("\\{p}"),
                String::new(),
            ] {
                assert_eq!(parse(&t, p), None, "{t:?}");
            }
        }
    }

    #[test]
    fn other_prefixes_do_not_ask() {
        assert_eq!(parse("? x", ','), None);
        assert_eq!(parse(", x", '?'), None);
        assert_eq!(parse("# note", '?'), None);
        assert_eq!(parse("#note", '#'), None); // stays a comment
        assert_eq!(parse("# note", '#'), q(1, "note"));
    }

    #[test]
    fn legacy_reset_works_for_the_question_mark_only() {
        assert_eq!(parse("?!", '?'), Some(Form::New));
        assert_eq!(
            parse("?! again", '?'),
            Some(Form::Question {
                blocks: 1,
                text: "again".into(),
                reset: true
            })
        );
        assert_eq!(parse(",!", ','), None);
    }

    #[test]
    fn trailing_blanks_are_ignored_for_the_letters() {
        for &p in ALLOWED {
            assert_eq!(parse(&format!("{p}n  "), p), Some(Form::New));
            assert_eq!(parse(&format!("{p}c\t"), p), Some(Form::Compact));
            assert_eq!(parse(&format!("{p}h "), p), Some(Form::Help));
            assert_eq!(parse(&format!("{p}3 "), p), q(3, ""));
        }
    }

    #[test]
    fn several_lines_are_never_a_question() {
        for &p in ALLOWED {
            for t in [
                format!("{p} why\nls"),
                format!("{p}\n"),
                format!("{p}n\r\n"),
                format!("{p}3 x\ny"),
            ] {
                assert_eq!(parse(&t, p), None, "{t:?}");
            }
        }
    }

    #[test]
    fn block_count_is_clamped() {
        assert_eq!(parse("?0", '?'), q(1, ""));
        assert_eq!(parse("?99 x", '?'), q(20, "x"));
        assert_eq!(parse("?99999999999999999999", '?'), q(20, ""));
    }

    #[test]
    fn config_value() {
        for &p in ALLOWED {
            assert_eq!(from_config(&p.to_string()), Ok(p));
        }
        for bad in [
            "", "??", ".", "/", "~", "!", "%", "^", ":", "-", "*", "\\", "$", "&", "a",
        ] {
            let e = from_config(bad).unwrap_err();
            assert!(e.contains("? , @ # ;"), "{e}");
        }
    }
}
