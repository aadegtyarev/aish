//! `?` lines. Not connected to a model yet: the line is kept on screen, a dim
//! notice follows and nothing reaches the shell.

use lineedit_wip::plugins::Intercept;
use lineedit_wip::{Class, Plugin, Span};

use crate::module::{Env, Module};

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

/// Paints the `?` marker in the accent color and the rest as plain text.
struct Accent;

impl Plugin for Accent {
    fn name(&self) -> &'static str {
        "model-accent"
    }
    fn priority(&self) -> i32 {
        50
    }
    fn highlight(&self, text: &str, out: &mut Vec<Span>) {
        if is_model_line(text) {
            out.push(Span {
                start: 1,
                end: text.len(),
                class: Class::Default,
            });
            out.push(Span {
                start: 0,
                end: 1,
                class: Class::Accent,
            });
        }
    }
}

pub struct ModelModule;

impl Module for ModelModule {
    fn name(&self) -> &'static str {
        "model"
    }

    fn editor_plugins(&mut self, _env: &Env) -> Vec<Box<dyn Plugin>> {
        vec![Box::new(Intercept::new(is_model_line)), Box::new(Accent)]
    }

    fn on_intercept(&mut self, line: &str, out: &mut Vec<u8>) -> bool {
        if !is_model_line(line) {
            return false;
        }
        out.extend_from_slice(" │ model not connected yet".as_bytes());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_lines() {
        for t in ["?", "? x", "?3 x", "?!", "?! ", "?12", "? "] {
            assert!(is_model_line(t), "{t}");
        }
        for t in ["?.txt", "??", "?x", "?3x", "?!x", "", "ls ?", " ?", "\\?"] {
            assert!(!is_model_line(t), "{t}");
        }
    }
}
