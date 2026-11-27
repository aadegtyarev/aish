use crate::highlight::{highlight, Span};
use crate::plugin::Plugin;

/// Token-based shell-like highlighting: the first word of a command is plain
/// if `resolver` accepts it and red otherwise, quoted strings, `$vars` and
/// comments get their own colors.
pub struct SyntaxHighlight {
    resolver: Box<dyn Fn(&str) -> bool + Send>,
}

impl SyntaxHighlight {
    /// `resolver` says whether a command word can be run.
    pub fn new(resolver: impl Fn(&str) -> bool + Send + 'static) -> Self {
        SyntaxHighlight {
            resolver: Box::new(resolver),
        }
    }
}

impl Plugin for SyntaxHighlight {
    fn name(&self) -> &'static str {
        "syntax-highlight"
    }
    fn priority(&self) -> i32 {
        -10
    }
    fn highlight(&self, text: &str, out: &mut Vec<Span>) {
        highlight(text, &*self.resolver, out);
    }
}
