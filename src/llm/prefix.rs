//! The forms a line may take with the question prefix (`?` by default).

/// A line that starts with the prefix, understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Form {
    /// `? text`, `?N`, `?N text`.
    Question { blocks: usize, text: String },
    /// `?` alone: chat mode on/off.
    Chat,
    /// `?n`
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
    let q = |blocks: usize, text: &str| Form::Question {
        blocks,
        text: text.trim().to_owned(),
    };
    if r.is_empty() {
        return Some(Form::Chat);
    }
    if r.starts_with(' ') {
        return Some(q(1, r));
    }
    match r {
        "n" => return Some(Form::New),
        "c" => return Some(Form::Compact),
        "h" => return Some(Form::Help),
        _ => {}
    }
    let digits = r.chars().take_while(char::is_ascii_digit).count();
    let (n, rest) = r.split_at(digits);
    if digits > 0 && (rest.is_empty() || rest.starts_with(' ')) {
        let blocks = n.parse().unwrap_or(20usize).clamp(1, 20);
        return Some(q(blocks, rest));
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
        (format!("{p}"), "chat mode on/off"),
        (format!("{p}N"), "about the last N blocks"),
        (format!("{p}n"), "new conversation"),
        (format!("{p}c"), "compact conversation"),
        (format!("{p}h"), "this help"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ALLOWED;

    fn q(blocks: usize, text: &str) -> Option<Form> {
        Some(Form::Question {
            blocks,
            text: text.into(),
        })
    }

    #[test]
    fn every_allowed_prefix_asks() {
        for &p in ALLOWED {
            assert_eq!(parse(&format!("{p}"), p), Some(Form::Chat), "{p}");
            assert_eq!(parse(&format!("{p}  "), p), Some(Form::Chat), "{p}");
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
}
