use std::sync::{Arc, RwLock};

use crate::history::History;
use crate::keys::Key;
use crate::plugin::{Ctx, Handled, Plugin, SubmitAction};

/// A history source shared between plugins and the host (which can swap in a
/// freshly loaded one from any thread).
#[derive(Clone)]
pub struct SharedHistory(Arc<RwLock<Box<dyn History>>>);

impl SharedHistory {
    /// Wrap a history source.
    pub fn new(h: Box<dyn History>) -> Self {
        SharedHistory(Arc::new(RwLock::new(h)))
    }
    /// Replace the source.
    pub fn set(&self, h: Box<dyn History>) {
        if let Ok(mut g) = self.0.write() {
            *g = h;
        }
    }
    pub(crate) fn with<R>(&self, f: impl FnOnce(&dyn History) -> R) -> Option<R> {
        self.0.read().ok().map(|g| f(&**g))
    }
    pub(crate) fn add(&self, line: &str) {
        if let Ok(mut g) = self.0.write() {
            g.add(line);
        }
    }
}

struct Nav {
    pos: usize,
    prefix: String,
    draft: String,
}

/// Ghost text from history, Up/Down (actions `history-prev` / `history-next`,
/// prefix-filtered when the line is not empty) and recording submitted lines.
pub struct HistorySuggest {
    hist: SharedHistory,
    nav: Option<Nav>,
    own_rev: u64,
    last: String,
}

impl HistorySuggest {
    /// Use `hist` as the source.
    pub fn new(hist: SharedHistory) -> Self {
        HistorySuggest {
            hist,
            nav: None,
            own_rev: u64::MAX,
            last: String::with_capacity(128),
        }
    }

    fn up(&mut self, ctx: &mut Ctx) {
        let mut nav = self.nav.take().unwrap_or_else(|| Nav {
            pos: self.hist.with(|h| h.len()).unwrap_or(0),
            prefix: ctx.text().to_owned(),
            draft: ctx.text().to_owned(),
        });
        let hit = self
            .hist
            .with(|h| {
                h.prev_matching(nav.pos, &nav.prefix, &nav.draft)
                    .map(|i| (i, h.get(i).to_owned()))
            })
            .flatten();
        if let Some((i, text)) = hit {
            nav.pos = i;
            ctx.set_text(&text);
        }
        self.own_rev = ctx.generation();
        self.nav = Some(nav);
    }

    fn down(&mut self, ctx: &mut Ctx) {
        let Some(mut nav) = self.nav.take() else {
            return;
        };
        let hit = self
            .hist
            .with(|h| {
                h.next_matching(nav.pos, &nav.prefix, &nav.draft)
                    .map(|i| (i, h.get(i).to_owned()))
            })
            .flatten();
        match hit {
            Some((i, text)) => {
                nav.pos = i;
                ctx.set_text(&text);
                self.nav = Some(nav);
            }
            None => ctx.set_text(&nav.draft),
        }
        self.own_rev = ctx.generation();
    }
}

impl Plugin for HistorySuggest {
    fn name(&self) -> &'static str {
        "history-suggest"
    }
    fn bindings(&self) -> &'static [(Key, &'static str)] {
        &[
            (Key::Up, "history-prev"),
            (Key::Ctrl(b'p'), "history-prev"),
            (Key::Down, "history-next"),
            (Key::Ctrl(b'n'), "history-next"),
        ]
    }
    fn on_action(&mut self, ctx: &mut Ctx, action: &str) -> Handled {
        match action {
            "history-prev" => self.up(ctx),
            "history-next" => self.down(ctx),
            _ => return Handled::Pass,
        }
        Handled::Yes
    }
    fn on_buffer_change(&mut self, ctx: &mut Ctx) {
        if ctx.generation() != self.own_rev {
            self.nav = None;
        }
    }
    fn suggest(&mut self, ctx: &mut Ctx) -> Option<&str> {
        self.hist.with(|h| {
            h.suggest(ctx.text()).map(|e| {
                self.last.clear();
                self.last.push_str(e);
            })
        })??;
        Some(&self.last)
    }
    fn on_submit(&mut self, _ctx: &mut Ctx, line: &str) -> SubmitAction {
        self.hist.add(line);
        self.nav = None;
        SubmitAction::Submit
    }
}
