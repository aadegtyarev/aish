//! Instant, local ghost text for llmsh's own command (`ai` / `llmsh`):
//! subcommands and model references. No model call; wins over history.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use lineedit_wip::{Ctx, Decoration, Plugin};

use super::Llm;

/// What the suggestions depend on.
pub struct State {
    /// Names the command goes by (`ai`, `llmsh`).
    pub names: Vec<String>,
    pub suggest_on: bool,
    /// Every `provider/alias`, in config order.
    pub models: Vec<String>,
    pub chat: Option<String>,
    pub complete: Option<String>,
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
                    ("off", "подсказки: сейчас вкл"),
                    ("on", "включить подсказки"),
                )
            } else {
                (
                    ("on", "подсказки: сейчас выкл"),
                    ("off", "выключить подсказки"),
                )
            };
            let subs = [
                ("status", "статус моделей"),
                ("chat", "сменить модель чата"),
                ("complete", "сменить модель подсказок"),
                toggle,
                other,
                ("config", "открыть конфиг"),
                ("init", "настроить запуск и модели"),
            ];
            // `o` suggests the toggle that does something, whichever it is
            let mut cands: Vec<(&str, &str)> = subs.to_vec();
            if part.starts_with('o') {
                cands.sort_by_key(|(n, _)| *n != toggle.0);
            }
            let (n, d) = cands
                .into_iter()
                .find(|(n, _)| n.starts_with(part) && n != part)?;
            Some((n[part.len()..].to_owned(), format!("· {d}")))
        }
        [role @ ("chat" | "complete"), part] => {
            let cur = if *role == "chat" {
                &st.chat
            } else {
                &st.complete
            };
            let mut it = st
                .models
                .iter()
                .filter(|m| m.starts_with(part) && *m != part);
            // the likely switch target first: not the current one
            let pick = it
                .clone()
                .find(|m| Some(*m) != cur.as_ref())
                .or_else(|| it.next());
            let pick = pick.filter(|m| Some(*m) != cur.as_ref())?;
            Some((
                pick[part.len()..].to_owned(),
                format!("· сейчас {}", cur.as_deref().unwrap_or("не задана")),
            ))
        }
        _ => None,
    }
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
        let mut names = vec!["llmsh".to_owned()];
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
                            a.into_iter()
                                .map(|m| format!("{}/{m}", p.id))
                                .collect::<Vec<_>>()
                        })
                        .collect()
                })
                .unwrap_or_default(),
            chat: self.llm.target("chat").map(|t| t.label),
            complete: self.llm.target("complete").map(|t| t.label),
        }
    }

    fn update(&mut self, line: &str) {
        let ours = line
            .split(' ')
            .next()
            .is_some_and(|f| f == "llmsh" || self.llm.command().as_deref() == Some(f));
        self.cur = if ours && line.contains(' ') {
            compute(&self.state(), line)
        } else {
            None
        };
        self.full = self
            .cur
            .as_ref()
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
            names: vec!["ai".into(), "llmsh".into()],
            suggest_on: on,
            models: vec![
                "cloud/main".into(),
                "local/big".into(),
                "local/coder".into(),
            ],
            chat: Some("cloud/main".into()),
            complete: Some("local/coder".into()),
        }
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
        assert_eq!(c(&s, "llmsh s").unwrap().0, "tatus");
        assert_eq!(c(&s, "ai x"), None);
        assert_eq!(c(&s, "ai status"), None);
        assert_eq!(c(&s, "ai"), None);
        assert_eq!(c(&s, "ls c"), None);
    }

    #[test]
    fn toggle_shows_the_opposite_state() {
        let on = c(&st(true), "ai o").unwrap();
        assert_eq!(on.0, "ff");
        assert!(on.1.contains("сейчас вкл"));
        let off = c(&st(false), "ai o").unwrap();
        assert_eq!(off.0, "n");
        assert!(off.1.contains("сейчас выкл"));
        // typed far enough to name the other one: that one
        assert_eq!(c(&st(false), "ai of").unwrap().0, "f");
        assert_eq!(c(&st(true), "ai on"), None);
    }

    #[test]
    fn model_refs_prefer_the_one_that_is_not_current() {
        let s = st(true);
        let r = c(&s, "ai chat ").unwrap();
        assert_eq!(r.0, "local/big");
        assert_eq!(r.1, "· сейчас cloud/main");
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
