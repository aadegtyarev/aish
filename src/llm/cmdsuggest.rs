//! Instant, local ghost text for aish's own command (`ai` / `aish`):
//! subcommands and model references. No model call; wins over history.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use aish_editor::{Ctx, Decoration, Plugin};

use super::Llm;
use crate::config::RunMode;

/// What the suggestions depend on.
pub struct State {
    /// Names the command goes by (`ai`, `aish`).
    pub names: Vec<String>,
    pub suggest_on: bool,
    /// Every `provider/alias`, in config order.
    pub models: Vec<String>,
    pub chat: Option<String>,
    pub complete: Option<String>,
    /// Profile names, and the active one.
    pub profiles: Vec<String>,
    pub profile: Option<String>,
    pub run: RunMode,
    /// Switchable tools and whether each is on.
    pub tools: Vec<(String, bool)>,
}

/// `--save` as the last word of `ai run ...` / `ai tools ...`.
fn save_flag(part: &str) -> Option<(String, String)> {
    ((part.is_empty() || part.starts_with('-')) && "--save".starts_with(part) && part != "--save")
        .then(|| {
            (
                "--save"[part.len()..].to_owned(),
                "· keep in the config".to_owned(),
            )
        })
}

/// `(rest of the line to append, short description)`.
pub fn compute(st: &State, line: &str) -> Option<(String, String)> {
    let mut it = line.split(' ');
    let first = it.next()?;
    if !st.names.iter().any(|n| n == first) {
        return None;
    }
    let words: Vec<&str> = it.collect();
    match words.as_slice() {
        [part] => {
            let (toggle, other) = if st.suggest_on {
                (
                    ("off", "suggestions: now on"),
                    ("on", "turn suggestions on"),
                )
            } else {
                (
                    ("on", "suggestions: now off"),
                    ("off", "turn suggestions off"),
                )
            };
            let subs = [
                ("status", "model status"),
                ("chat", "switch the chat model"),
                ("complete", "switch the suggestion model"),
                ("model", "switch a role's model"),
                ("models", "list models"),
                ("profile", "profiles"),
                ("run", "may the model run commands"),
                ("tools", "tools on/off"),
                toggle,
                other,
                ("config", "open the config"),
                ("init", "set up launch and models"),
                ("help", "list commands"),
            ];
            // a full command name is not completed into a longer one
            if subs.iter().any(|(n, _)| n == part) {
                return None;
            }
            // `o` suggests the toggle that does something, whichever it is
            let mut cands: Vec<(&str, &str)> = subs.to_vec();
            if part.starts_with('o') {
                cands.sort_by_key(|(n, _)| *n != toggle.0);
            }
            let found = cands
                .into_iter()
                .find(|(n, _)| n.starts_with(part) && n != part);
            match found {
                Some((n, d)) => Some((n[part.len()..].to_owned(), format!("· {d}"))),
                // not a prefix of anything, but close to one: only a hint
                None if part.chars().all(|c| c.is_ascii_alphabetic()) => {
                    super::cmd::closest(part, subs.iter().map(|s| s.0))
                        .map(|n| (String::new(), format!("→ {n}")))
                }
                None => None,
            }
        }
        [role @ ("chat" | "complete"), part] | ["model", role @ ("chat" | "complete"), part] => {
            complete_ref(st, role, part)
        }
        ["model", part] => {
            // a role name, or a model for chat
            let role = ["chat", "complete"]
                .into_iter()
                .find(|r| r.starts_with(part) && r != part && (!part.is_empty() || *r == "chat"));
            match role {
                Some(r) => Some((r[part.len()..].to_owned(), "· role".to_owned())),
                None => complete_ref(st, "chat", part),
            }
        }
        ["run", part] => {
            // a mode other than the current one
            let m = RunMode::ALL
                .into_iter()
                .filter(|m| m.name().starts_with(part) && m.name() != *part)
                .find(|m| *m != st.run)?;
            Some((
                m.name()[part.len()..].to_owned(),
                format!("· now {}", st.run.name()),
            ))
        }
        ["run", m, part] if RunMode::parse(m).is_some() => save_flag(part),
        ["tools", part] => {
            let (n, on) = st
                .tools
                .iter()
                .find(|(n, _)| n.starts_with(part) && n != part)?;
            Some((
                n[part.len()..].to_owned(),
                format!("· now {}", if *on { "on" } else { "off" }),
            ))
        }
        ["tools", name, part] => {
            let on = st.tools.iter().find(|(n, _)| n == name)?.1;
            // the other state first
            let order = if on { ["off", "on"] } else { ["on", "off"] };
            let w = order
                .into_iter()
                .find(|w| w.starts_with(part) && w != part)?;
            Some((
                w[part.len()..].to_owned(),
                format!("· now {}", if on { "on" } else { "off" }),
            ))
        }
        ["tools", name, state @ ("on" | "off"), part]
            if st.tools.iter().any(|(n, _)| n == name) =>
        {
            let _ = state;
            save_flag(part)
        }
        ["profile", part] => {
            let pick = st
                .profiles
                .iter()
                .filter(|p| p.starts_with(part) && *p != part)
                .find(|p| st.profile.as_ref() != Some(p))?;
            Some((
                pick[part.len()..].to_owned(),
                format!("· now {}", st.profile.as_deref().unwrap_or("none")),
            ))
        }
        _ => None,
    }
}

/// A model reference for `role`: the likely switch target first (not the
/// current one).
fn complete_ref(st: &State, role: &str, part: &str) -> Option<(String, String)> {
    let cur = if role == "chat" {
        &st.chat
    } else {
        &st.complete
    };
    // `m` names the current model, as `provider/alias` or bare alias
    let is_cur = |m: &str| {
        cur.as_deref().is_some_and(|c| {
            c == m || (!m.contains('/') && c.split_once('/').is_some_and(|(_, a)| a == m))
        })
    };
    let mut it = st
        .models
        .iter()
        .filter(|m| m.starts_with(part) && *m != part);
    let pick = it.clone().find(|m| !is_cur(m)).or_else(|| it.next());
    let pick = pick.filter(|m| !is_cur(m))?;
    Some((
        pick[part.len()..].to_owned(),
        format!("· now {}", cur.as_deref().unwrap_or("unset")),
    ))
}

pub struct CmdSuggest {
    llm: Arc<Llm>,
    cur: Option<(String, String)>,
    full: String,
}

impl CmdSuggest {
    pub fn new(llm: Arc<Llm>) -> Self {
        CmdSuggest {
            llm,
            cur: None,
            full: String::new(),
        }
    }

    fn state(&self) -> State {
        let mut names = vec!["aish".to_owned()];
        names.extend(self.llm.command());
        State {
            names,
            suggest_on: self.llm.suggest_on.load(Ordering::Relaxed),
            models: self
                .llm
                .config()
                .map(|c| {
                    c.providers
                        .iter()
                        .flat_map(|p| {
                            let mut a: Vec<&String> = p.models.keys().collect();
                            a.sort();
                            // `provider/alias`, plus the bare alias when no
                            // other provider has it
                            a.into_iter()
                                .flat_map(|m| {
                                    let full = format!("{}/{m}", p.id);
                                    if c.providers_with(m).len() == 1 {
                                        vec![m.clone(), full]
                                    } else {
                                        vec![full]
                                    }
                                })
                                .collect::<Vec<_>>()
                        })
                        .collect()
                })
                .unwrap_or_default(),
            chat: self.llm.target("chat").map(|t| t.label),
            complete: self.llm.target("complete").map(|t| t.label),
            profiles: self
                .llm
                .config()
                .map(|c| c.profiles.0.iter().map(|(n, _)| n.clone()).collect())
                .unwrap_or_default(),
            profile: self.llm.config().and_then(|c| c.profile.clone()),
            run: self.llm.run_mode(),
            tools: crate::config::SWITCHABLE_TOOLS
                .iter()
                .map(|n| ((*n).to_owned(), self.llm.tool_on(n)))
                .collect(),
        }
    }

    fn update(&mut self, line: &str) {
        let ours = line
            .split(' ')
            .next()
            .is_some_and(|f| f == "aish" || self.llm.command().as_deref() == Some(f));
        self.cur = if ours && line.contains(' ') {
            compute(&self.state(), line)
        } else {
            None
        };
        self.full = self
            .cur
            .as_ref()
            .filter(|(r, _)| !r.is_empty())
            .map(|(r, _)| format!("{line}{r}"))
            .unwrap_or_default();
    }
}

impl Plugin for CmdSuggest {
    fn name(&self) -> &'static str {
        "cmd-suggest"
    }
    /// Above history and the model.
    fn priority(&self) -> i32 {
        10
    }
    fn suggest(&mut self, ctx: &mut Ctx) -> Option<&str> {
        let t = ctx.text().to_owned();
        self.update(&t);
        (!self.full.is_empty()).then_some(self.full.as_str())
    }
    fn decorate(&mut self, ctx: &mut Ctx, _width: u16, out: &mut Decoration) {
        let t = ctx.text().to_owned();
        self.update(&t);
        if let Some((_, d)) = &self.cur {
            out.right_hint(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(on: bool) -> State {
        State {
            names: vec!["ai".into(), "aish".into()],
            suggest_on: on,
            models: vec![
                "cloud/main".into(),
                "local/big".into(),
                "local/coder".into(),
            ],
            chat: Some("cloud/main".into()),
            complete: Some("local/coder".into()),
            profiles: vec!["local".into(), "inet".into()],
            profile: Some("local".into()),
            run: RunMode::Ask,
            tools: vec![
                ("run".into(), true),
                ("files".into(), true),
                ("web".into(), false),
            ],
        }
    }

    #[test]
    fn run_and_tools_args() {
        let s = st(true);
        assert_eq!(c(&s, "ai r").unwrap().0, "un");
        assert_eq!(c(&s, "ai t").unwrap().0, "ools");
        // not the current mode
        assert_eq!(
            c(&s, "ai run ").unwrap(),
            ("judge".into(), "· now ask".into())
        );
        assert_eq!(c(&s, "ai run j").unwrap().0, "udge");
        assert_eq!(c(&s, "ai run o").unwrap().0, "ff");
        assert_eq!(c(&s, "ai run as"), None);
        let off = State {
            run: RunMode::Off,
            ..st(true)
        };
        assert_eq!(c(&off, "ai run ").unwrap().0, "ask");
        assert_eq!(c(&s, "ai run judge ").unwrap().0, "--save");
        assert_eq!(c(&s, "ai run judge --sa").unwrap().0, "ve");
        assert_eq!(c(&s, "ai run x --sa"), None);
        assert_eq!(
            c(&s, "ai tools ").unwrap(),
            ("run".into(), "· now on".into())
        );
        assert_eq!(
            c(&s, "ai tools w").unwrap(),
            ("eb".into(), "· now off".into())
        );
        // the opposite state first
        assert_eq!(c(&s, "ai tools web ").unwrap().0, "on");
        assert_eq!(c(&s, "ai tools run ").unwrap().0, "off");
        assert_eq!(c(&s, "ai tools run o").unwrap().0, "ff");
        assert_eq!(c(&s, "ai tools run off ").unwrap().0, "--save");
        assert_eq!(c(&s, "ai tools nope "), None);
    }

    #[test]
    fn bare_alias_and_current_given_as_provider_alias() {
        let s = State {
            models: vec![
                "mellum".into(),
                "local/mellum".into(),
                "oss".into(),
                "local/oss".into(),
            ],
            chat: Some("local/mellum".into()),
            ..st(true)
        };
        // the current one (bare form) is not offered
        assert_eq!(c(&s, "ai chat mel"), None);
        assert_eq!(c(&s, "ai chat o").unwrap().0, "ss");
        assert_eq!(c(&s, "ai chat local/o").unwrap().0, "ss");
    }

    fn c(s: &State, l: &str) -> Option<(String, String)> {
        compute(s, l)
    }

    #[test]
    fn subcommands() {
        let s = st(true);
        assert_eq!(c(&s, "ai ").unwrap().0, "status");
        assert_eq!(c(&s, "ai c").unwrap().0, "hat");
        assert_eq!(c(&s, "ai co").unwrap().0, "mplete");
        assert_eq!(c(&s, "ai con").unwrap().0, "fig");
        assert_eq!(c(&s, "aish s").unwrap().0, "tatus");
        assert_eq!(c(&s, "ai x"), None);
        // a typo: no ghost text, only the hint
        assert_eq!(c(&s, "ai chst"), Some((String::new(), "→ chat".into())));
        assert_eq!(c(&s, "ai тест"), None);
        assert_eq!(c(&s, "ai status"), None);
        assert_eq!(c(&s, "ai"), None);
        assert_eq!(c(&s, "ls c"), None);
    }

    #[test]
    fn new_subcommands_and_args() {
        let s = st(true);
        assert_eq!(c(&s, "ai mod").unwrap().0, "el");
        assert_eq!(c(&s, "ai model"), None);
        assert_eq!(c(&s, "ai models"), None);
        assert_eq!(c(&s, "ai pro").unwrap().0, "file");
        assert_eq!(c(&s, "ai he").unwrap().0, "lp");
        // roles, then models for chat
        assert_eq!(c(&s, "ai model ").unwrap().0, "chat");
        assert_eq!(c(&s, "ai model co").unwrap().0, "mplete");
        assert_eq!(c(&s, "ai model lo").unwrap().0, "cal/big");
        assert_eq!(c(&s, "ai model chat ").unwrap().0, "local/big");
        assert_eq!(c(&s, "ai model complete ").unwrap().0, "cloud/main");
        assert_eq!(c(&s, "ai model complete cl").unwrap().0, "oud/main");
        assert_eq!(c(&s, "ai model chat a b"), None);
        // the active profile is not offered
        assert_eq!(c(&s, "ai profile ").unwrap().0, "inet");
        assert_eq!(c(&s, "ai profile l"), None);
        assert_eq!(c(&s, "ai profile i").unwrap().0, "net");
    }

    #[test]
    fn toggle_shows_the_opposite_state() {
        let on = c(&st(true), "ai o").unwrap();
        assert_eq!(on.0, "ff");
        assert!(on.1.contains("now on"));
        let off = c(&st(false), "ai o").unwrap();
        assert_eq!(off.0, "n");
        assert!(off.1.contains("now off"));
        // typed far enough to name the other one: that one
        assert_eq!(c(&st(false), "ai of").unwrap().0, "f");
        assert_eq!(c(&st(true), "ai on"), None);
    }

    #[test]
    fn model_refs_prefer_the_one_that_is_not_current() {
        let s = st(true);
        let r = c(&s, "ai chat ").unwrap();
        assert_eq!(r.0, "local/big");
        assert_eq!(r.1, "· now cloud/main");
        assert_eq!(c(&s, "ai chat lo").unwrap().0, "cal/big");
        // only the current one matches: nothing to suggest
        assert_eq!(c(&s, "ai chat cl"), None);
        // role `complete`: current is local/coder, so the first other one
        assert_eq!(c(&s, "ai complete ").unwrap().0, "cloud/main");
        assert_eq!(c(&s, "ai complete local/c"), None);
        assert_eq!(c(&s, "ai chat local/big"), None);
        assert_eq!(c(&s, "ai chat a b"), None);
    }
}
