//! Session settings of the agent's `run` mode and per-tool switches: the
//! config gives the default, `aish run` / `aish tools` override it for the session.

use std::collections::HashMap;

use super::Llm;
use crate::config::{Coach, RunMode};

#[derive(Default)]
pub struct Opts {
    run: Option<RunMode>,
    coach: Option<Coach>,
    tools: HashMap<String, bool>,
}

impl Llm {
    /// What the `run` tool does with commands that are not read-only.
    pub fn run_mode(&self) -> RunMode {
        let over = self.opts.lock().ok().and_then(|o| o.run);
        over.unwrap_or_else(|| self.config().map_or(RunMode::Ask, |c| c.run_mode()))
    }

    pub fn set_run_mode(&self, m: RunMode) {
        if let Ok(mut o) = self.opts.lock() {
            o.run = Some(m);
        }
    }

    /// The language coach (`aish coach`, `coach:`).
    pub fn coach(&self) -> Coach {
        let over = self.opts.lock().ok().and_then(|o| o.coach);
        over.unwrap_or_else(|| self.config().map_or(Coach::Off, |c| c.coach_mode()))
    }

    pub fn set_coach(&self, m: Coach) {
        if let Ok(mut o) = self.opts.lock() {
            o.coach = Some(m);
        }
    }

    /// Is the tool offered to the model? (`run`, `files`, `web`, `skills`.)
    pub fn tool_on(&self, name: &str) -> bool {
        let over = self
            .opts
            .lock()
            .ok()
            .and_then(|o| o.tools.get(name).copied());
        over.unwrap_or_else(|| self.config().is_none_or(|c| c.tools.on(name)))
    }

    pub fn set_tool(&self, name: &str, on: bool) {
        if let Ok(mut o) = self.opts.lock() {
            o.tools.insert(name.to_owned(), on);
        }
    }
}
