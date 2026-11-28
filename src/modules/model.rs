//! `?` questions and the `aish` command: the line stays on screen, the reply
//! follows (dim `│` rows) and nothing reaches the shell.

use lineedit_wip::plugins::{HintText, Intercept, RightHint};
use lineedit_wip::{Class, Ctx, Handled, Plugin, Span};

use std::sync::{Arc, Mutex};

use crate::blocks::BlockTracker;
use crate::editor::overlay::toggle_prefix;
use crate::llm::agent;
use crate::llm::cmd::{self, is_aish_line, is_short_line, Cmd};
use crate::llm::render::{fit_line, Renderer};
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

/// Editor action bound to the ask key (see `keys.ask`): Ctrl-Q at the prompt
/// puts a `? ` in front of the line, or takes it away again.
pub const ASK_TOGGLE: &str = "ask-toggle";

struct AskToggle;

impl Plugin for AskToggle {
    fn name(&self) -> &'static str {
        ASK_TOGGLE
    }
    fn on_action(&mut self, ctx: &mut Ctx, action: &str) -> Handled {
        if action != ASK_TOGGLE {
            return Handled::Pass;
        }
        let (text, cursor) = toggle_prefix(ctx.text(), ctx.cursor());
        ctx.set_text(&text);
        ctx.set_cursor(cursor);
        Handled::Yes
    }
}

#[derive(Default)]
pub struct ModelModule {
    env: Option<(Arc<Llm>, Arc<Mutex<BlockTracker>>)>,
    ls: Arc<LsCache>,
}

/// Lines the model module takes: `?`, `aish ...` and the configured short
/// command (`ai`), which wins over anything of that name in the system.
fn claims(line: &str, llm: &Llm) -> bool {
    is_model_line(line) || is_aish_line(line) || is_short_line(line, llm.command().as_deref())
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
            Box::new(AskToggle),
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
            return aish_command(&llm, line, out, task);
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
                out.extend_from_slice("│ conversation reset".as_bytes());
                return true;
            }
        }
        let Some(target) = llm.target("chat") else {
            let n = format!("│ no model configured · {} config", llm.command_label());
            out.extend_from_slice(n.as_bytes());
            return true;
        };
        match task.take() {
            Some(t) => ask::spawn(llm, tracker, target, p, t),
            None => out.extend_from_slice("│ busy".as_bytes()),
        }
        true
    }
}

fn aish_command(llm: &Arc<Llm>, line: &str, out: &mut Vec<u8>, task: &mut Option<Task>) -> bool {
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
        Cmd::Init => match task.take() {
            Some(t) => t.finish(Some("aish init".into())),
            None => return false,
        },
        Cmd::Config => match task.take() {
            Some(t) => t.finish(Some(cmd::config_command(llm))),
            None => return false,
        },
        _ => {
            let Some(t) = task.take() else {
                // busy: only the cheap switches work without a task
                let ls = cmd::run(llm, &cmd, 34);
                let text: Vec<String> = ls.iter().map(|l| format!("{}{}", l.0, l.1)).collect();
                out.extend_from_slice(format!("│ {}", text.join(" ")).as_bytes());
                return true;
            };
            // (on a worker, like status: the editor finishes its own line first)
            let (llm, cmd) = (llm.clone(), cmd);
            std::thread::spawn(move || {
                let r = Renderer::new(&t.theme, t.cols);
                let w = usize::from(t.cols).saturating_sub(3).max(10);
                let mut b = Vec::new();
                for l in cmd::run(&llm, &cmd, w).iter() {
                    let (h, d) = (fit_line(&l.0, w + 1), &l.1);
                    let room = (w + 1).saturating_sub(h.chars().count());
                    let d = fit_line(d, room.max(1));
                    r.split_row(&h, &d, &mut b);
                }
                t.write(&b);
                t.finish(None);
            });
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
