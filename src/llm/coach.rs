//! The language coach: a correction of the user's question, asked from the
//! `complete` model in parallel with the turn.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use aish_llm::{Message, Request};

use super::agent::reply_language;
use super::mask::mask;
use super::render::Renderer;
use super::Llm;
use crate::config::Coach;
use crate::task::Task;

/// How long the end of a turn waits for a correction that is not ready.
pub const WAIT: Duration = Duration::from_secs(2);

/// Letters and digits, lowercase: what makes two questions "the same".
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The pending correction of one question.
pub struct Fix(Option<Receiver<String>>);

impl Fix {
    /// Ask for the correction of `q` now (nothing for an empty question, `coach: off`,
    /// or a non-ASCII question when the language is English: it would be translated).
    pub fn start(llm: &Llm, q: &str) -> Fix {
        let lang = reply_language(llm.config().as_deref());
        let english = lang.eq_ignore_ascii_case("english");
        let t = llm.targets_up("complete").into_iter().next();
        let (Some(t), true) = (t, llm.coach() != Coach::Off && !q.trim().is_empty()) else {
            return Fix(None);
        };
        if english && q.chars().any(|c| c.is_alphabetic() && !c.is_ascii()) {
            return Fix(None);
        }
        let q = if t.local {
            q.trim().to_owned()
        } else {
            mask(q.trim())
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let req = Request {
                max_tokens: 120,
                temperature: Some(0.2),
                deadline: Some(Instant::now() + 8 * WAIT),
                ..Request::chat(
                    format!(
                        "Correct the spelling and grammar of the user's {lang} question. Reply with \
ONLY the corrected question, or exactly OK if it is already correct or not in {lang}."
                    ),
                    vec![Message::user(q.clone())],
                )
            };
            let Ok(r) = aish_llm::complete(&t.ep, &req, &AtomicBool::new(false)) else {
                return;
            };
            let r = r.trim().lines().next().unwrap_or("").trim();
            if r != "OK" && !norm(r).is_empty() && norm(r) != norm(&q) {
                let _ = tx.send(r.to_owned());
            }
        });
        Fix(Some(rx))
    }

    /// Draw the correction if it is ready (or comes within `wait`); once.
    pub fn show(&mut self, task: &Task, wait: Duration) {
        let Some(rx) = &self.0 else { return };
        let line = match rx.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => return,
            r => r.ok(),
        };
        self.0 = None;
        if let Some(l) = line {
            let mut out = Vec::new();
            Renderer::new(&task.theme, task.cols).better(&l, &mut out);
            task.write(&out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_question_modulo_case_and_punctuation() {
        assert_eq!(
            norm("What does `chmod 755` do?"),
            norm("what does chmod 755 do?")
        );
        assert_ne!(norm("how to list file"), norm("how to list files"));
    }
}
