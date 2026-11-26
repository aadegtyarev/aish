//! Background jobs of the shell, as far as llmsh knows them (reported by the
//! rc hook, see `rc.bash`), and the logs of the ones the agent started.

use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u32,
    pub command: String,
    /// `Running`, `Stopped`, ...
    pub state: String,
    pub started: Instant,
}

#[derive(Debug, Default)]
pub struct Jobs {
    pub list: Vec<Job>,
    /// Logs of agent-started jobs by job number (`<command> &>log &`).
    pub links: std::collections::HashMap<u32, PathBuf>,
    pub seq: u64,
}

impl Jobs {
    /// One line per job for the model.
    pub fn describe(&self) -> String {
        if self.list.is_empty() {
            return "no background jobs".into();
        }
        self.list
            .iter()
            .map(|j| {
                format!(
                    "[{}] {} {} (running {}s){}",
                    j.id,
                    j.state,
                    j.command,
                    j.started.elapsed().as_secs(),
                    if self.links.contains_key(&j.id) {
                        " — log available (job_output)"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn link(&mut self, id: u32, log: PathBuf) {
        self.links.insert(id, log);
    }

    pub fn log_of(&self, id: u32) -> Option<PathBuf> {
        self.links.get(&id).cloned()
    }
}
