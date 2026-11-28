//! Questions (`?` or the configured prefix) and the `aish` command: the line
//! stays on screen, the reply follows (dim `│` rows) and nothing reaches the shell.

use aish_editor::plugins::{HintText, Intercept, RightHint};
use aish_editor::{Class, Ctx, Handled, Plugin, Span};

use std::sync::{Arc, Mutex};

use crate::blocks::BlockTracker;
use crate::editor::overlay::toggle_prefix;
use crate::llm::agent;
use crate::llm::cmd::{self, is_aish_line, is_short_line, Cmd};
use crate::llm::prefix::{self, is_model_line, Form};
use crate::llm::render::{fit_line, Renderer};
use crate::llm::suggest::{LsCache, ModelSuggest};
use crate::llm::{ask, compact, Llm};
use crate::module::{Env, Module};
use crate::task::Task;

/// Paints the prefix in the accent color and the rest as plain text.
struct Accent(char);

impl Plugin for Accent {
    fn name(&self) -> &'static str {
        "model-accent"
    }
    fn priority(&self) -> i32 {
        50
    }
    fn highlight(&self, text: &str, out: &mut Vec<Span>) {
        if is_model_line(text, self.0) {
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

struct AskToggle(char);

impl Plugin for AskToggle {
    fn name(&self) -> &'static str {
        ASK_TOGGLE
    }
    fn on_action(&mut self, ctx: &mut Ctx, action: &str) -> Handled {
        if action != ASK_TOGGLE {
            return Handled::Pass;
        }
        let (text, cursor) = toggle_prefix(ctx.text(), ctx.cursor(), self.0);
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

/// Lines the model module takes: questions, `aish ...` and the configured
/// short command (`ai`), which wins over anything of that name in the system.
fn claims(line: &str, llm: &Llm) -> bool {
    is_model_line(line, llm.prefix())
        || is_aish_line(line)
        || is_short_line(line, llm.command().as_deref())
}

impl Module for ModelModule {
    fn name(&self) -> &'static str {
        "model"
    }

    fn editor_plugins(&mut self, env: &Env) -> Vec<Box<dyn Plugin>> {
        self.env = Some((env.llm.clone(), env.tracker.clone()));
        let llm = env.llm.clone();
        let p = env.llm.prefix();
        let hint = HintText::new(&p.to_string());
        let _ = env
            .llm
            .status
            .set(crate::llm::status::Status::new(hint.clone(), p));
        vec![
            Box::new(Intercept::new(move |l| {
                claims(l, &llm) || (agent::asking(&llm) && !l.trim().is_empty())
            })),
            Box::new(RightHint::new(hint)),
            Box::new(Accent(p)),
            Box::new(AskToggle(p)),
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
        if agent::asking(&llm) && !claims(line, &llm) {
            agent::answer(&llm, line);
            let note = if line.trim().is_empty() {
                "skipped"
            } else {
                "answer sent"
            };
            out.extend_from_slice(format!("│ {note}").as_bytes());
            return true;
        }
        if !claims(line, &llm) {
            return false;
        }
        let Some(form) = prefix::parse(line, llm.prefix()) else {
            return aish_command(&llm, line, out, task);
        };
        if form == Form::Help {
            let Some(t) = task.take() else {
                out.extend_from_slice("│ busy".as_bytes());
                return true;
            };
            let p = llm.prefix();
            ask::run_task(t, move |t| {
                let w = usize::from(t.cols).saturating_sub(3).max(10);
                write_rows(t, &cmd::form_lines(w, p));
                None
            });
            return true;
        }
        if let Ok(mut pend) = llm.pending.lock() {
            *pend = None; // a new question ends a paused turn
        }
        if form == Form::New {
            if !llm.reset_memory() {
                out.extend_from_slice("◇ busy · Ctrl-C first".as_bytes());
                return true;
            }
            out.extend_from_slice("◇ new conversation".as_bytes());
            return true;
        }
        let Some(target) = llm.target("chat") else {
            let n = format!("│ no model configured · {} config", llm.command_label());
            out.extend_from_slice(n.as_bytes());
            return true;
        };
        let Some(t) = task.take() else {
            out.extend_from_slice("│ busy".as_bytes());
            return true;
        };
        match form {
            Form::Compact => ask::run_task(t, move |t| {
                let _turn = llm.turn.lock().unwrap_or_else(|e| e.into_inner());
                compact::run_command(&llm, &target, t);
                None
            }),
            Form::Question { blocks, text, .. } => {
                let p = ask::Parsed {
                    blocks,
                    question: text,
                };
                ask::spawn(llm, tracker, target, p, t);
            }
            Form::New | Form::Help => {}
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
            ask::run_task(t, move |t| {
                let r = Renderer::new(&t.theme, t.cols);
                let mut b = Vec::new();
                for l in cmd::status_lines(&llm) {
                    r.note(&l, &mut b);
                }
                t.write(&b);
                None
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
            ask::run_task(t, move |t| {
                let w = usize::from(t.cols).saturating_sub(3).max(10);
                write_rows(t, &cmd::run(&llm, &cmd, w));
                None
            });
        }
    }
    true
}

/// Rows of a command's output, each cut to the width.
fn write_rows(t: &Task, lines: &[cmd::Ln]) {
    let r = Renderer::new(&t.theme, t.cols);
    let w = usize::from(t.cols).saturating_sub(3).max(10);
    let mut b = Vec::new();
    for l in lines {
        let (h, d) = (fit_line(&l.0, w + 1), &l.1);
        let room = (w + 1).saturating_sub(h.chars().count());
        let d = fit_line(d, room.max(1));
        r.split_row(&h, &d, &mut b);
    }
    t.write(&b);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_lines() {
        for t in ["?", "? x", "?3 x", "?12", "? ", "?n", "?c", "?h"] {
            assert!(is_model_line(t, '?'), "{t}");
        }
        for t in [
            "?.txt", "??", "?x", "?3x", "?!", "?! x", "?!x", "", "ls ?", " ?", "\\?",
        ] {
            assert!(!is_model_line(t, '?'), "{t}");
        }
        assert!(is_model_line(", x", ',') && !is_model_line("? x", ','));
    }
}
