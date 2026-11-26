//! `?` questions and the `llmsh` command: the line stays on screen, the reply
//! follows (dim `│` rows) and nothing reaches the shell.

use lineedit_wip::plugins::{HintText, Intercept, RightHint};
use lineedit_wip::{Class, Plugin, Span};

use std::sync::{Arc, Mutex};

use crate::blocks::BlockTracker;
use crate::llm::agent;
use crate::llm::cmd::{self, is_llmsh_line, is_short_line, Cmd};
use crate::llm::render::Renderer;
use crate::llm::suggest::{LsCache, ModelSuggest};
use crate::llm::{ask, Llm};
use crate::module::{Env, Module, Task};

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

#[derive(Default)]
pub struct ModelModule {
    env: Option<(Arc<Llm>, Arc<Mutex<BlockTracker>>)>,
    ls: Arc<LsCache>,
}

/// Lines the model module takes: `?`, `llmsh ...` and the configured short
/// command (`ai`), which wins over anything of that name in the system.
fn claims(line: &str, llm: &Llm) -> bool {
    is_model_line(line) || is_llmsh_line(line) || is_short_line(line, llm.command().as_deref())
}

impl Module for ModelModule {
    fn name(&self) -> &'static str {
        "model"
    }

    fn editor_plugins(&mut self, env: &Env) -> Vec<Box<dyn Plugin>> {
        self.env = Some((env.llm.clone(), env.tracker.clone()));
        let llm = env.llm.clone();
        let hint = HintText::new("?");
        let _ = env
            .llm
            .status
            .set(crate::llm::status::Status::new(hint.clone()));
        vec![
            Box::new(Intercept::new(move |l| claims(l, &llm))),
            Box::new(RightHint::new(hint)),
            Box::new(Accent),
            Box::new(crate::llm::cmdsuggest::CmdSuggest::new(env.llm.clone())),
            Box::new(ModelSuggest::new(
                env.llm.clone(),
                env.history.clone(),
                env.tracker.clone(),
                self.ls.clone(),
            )),
        ]
    }

    fn on_block_finished(&mut self, block: &crate::blocks::Block) {
        self.ls.invalidate();
        if let Some((llm, _)) = &self.env {
            agent::on_block(llm, block);
        }
    }

    fn on_resume(&mut self, task: &mut Option<Task>) -> bool {
        let Some((llm, tracker)) = self.env.clone() else {
            return false;
        };
        if !agent::resumable(&llm) {
            return false;
        }
        let Some(t) = task.take() else { return false };
        ask::run_task(t, move |t| agent::resume(&llm, &tracker, t));
        true
    }

    fn on_intercept(&mut self, line: &str, out: &mut Vec<u8>, task: &mut Option<Task>) -> bool {
        let Some((llm, tracker)) = self.env.clone() else {
            return false;
        };
        if !claims(line, &llm) {
            return false;
        }
        if !is_model_line(line) {
            return llmsh_command(&llm, line, out, task);
        }
        let p = ask::parse(line);
        if let Ok(mut pend) = llm.pending.lock() {
            *pend = None; // a new question ends a paused turn
        }
        if p.reset {
            if let Ok(mut c) = llm.convo.lock() {
                c.clear();
            }
            if p.question.is_empty() {
                out.extend_from_slice("│ диалог сброшен".as_bytes());
                return true;
            }
        }
        let Some(target) = llm.target("chat") else {
            let n = format!("│ модель не настроена · {} config", llm.command_label());
            out.extend_from_slice(n.as_bytes());
            return true;
        };
        match task.take() {
            Some(t) => ask::spawn(llm, tracker, target, p, t),
            None => out.extend_from_slice("│ занято".as_bytes()),
        }
        true
    }
}

fn llmsh_command(llm: &Arc<Llm>, line: &str, out: &mut Vec<u8>, task: &mut Option<Task>) -> bool {
    let cmd = cmd::parse(line);
    match cmd {
        Cmd::Status => {
            let Some(t) = task.take() else { return false };
            let llm = llm.clone();
            std::thread::spawn(move || {
                let r = Renderer::new(&t.theme, t.cols);
                let mut b = Vec::new();
                for l in cmd::status_lines(&llm) {
                    r.note(&l, &mut b);
                }
                t.write(&b);
                t.finish(None);
            });
        }
        Cmd::Config => match task.take() {
            Some(t) => t.finish(Some(cmd::config_command(llm))),
            None => return false,
        },
        _ => {
            out.extend_from_slice(format!("│ {}", cmd::apply(llm, &cmd)).as_bytes());
        }
    }
    true
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
