use super::history::SharedHistory;
use crate::history::fuzzy;
use crate::keys::Key;
use crate::plugin::{Ctx, Decoration, Handled, Plugin, RowStyle};

const ROWS: usize = 8;

struct State {
    query: String,
    /// Every entry matching `query` as a subsequence (None = all, for the
    /// empty query). Appending a character only narrows it.
    pool: Option<Vec<usize>>,
    /// Best matches, best first.
    results: Vec<usize>,
    sel: usize,
}

/// Ctrl-R: inline fuzzy history search below the input (no alternate
/// screen). Action `history-search`. Enter puts the selection in the line,
/// Esc / Ctrl-G cancel.
pub struct HistorySearch {
    hist: SharedHistory,
    st: Option<State>,
    pos: Vec<usize>,
}

impl HistorySearch {
    /// Search in `hist`.
    pub fn new(hist: SharedHistory) -> Self {
        HistorySearch {
            hist,
            st: None,
            pos: Vec::new(),
        }
    }

    /// Recompute results. `narrow`: the query only grew, so the old pool is a
    /// superset of the new matches.
    fn requery(&mut self, narrow: bool) {
        let Some(s) = &mut self.st else { return };
        let q = s.query.clone();
        let ci = !q.bytes().any(|b| b.is_ascii_uppercase());
        self.hist.with(|h| {
            let n = h.len();
            if q.is_empty() {
                s.pool = None;
                s.results.clear();
                s.results.extend((0..n).rev().take(ROWS));
                return;
            }
            let old = if narrow { s.pool.take() } else { None };
            let candidates: Box<dyn Iterator<Item = usize>> = match old {
                Some(p) => Box::new(p.into_iter()),
                None => Box::new(0..n),
            };
            let mut pool = Vec::new();
            let mut top: Vec<(i32, usize)> = Vec::with_capacity(ROWS + 1);
            for i in candidates {
                let e = h.get(i);
                if e.contains('\n') {
                    continue;
                }
                if let Some(m) = fuzzy(q.as_bytes(), e.as_bytes(), ci, None) {
                    pool.push(i);
                    let sc = m + (i * 24 / n.max(1)) as i32;
                    if top.len() == ROWS && (sc, i) <= *top.last().unwrap_or(&(i32::MIN, 0)) {
                        continue;
                    }
                    let at = top.partition_point(|&(ts, ti)| (ts, ti) > (sc, i));
                    top.insert(at, (sc, i));
                    top.truncate(ROWS);
                }
            }
            s.pool = Some(pool);
            s.results = top.iter().map(|&(_, i)| i).collect();
        });
        s.sel = 0;
    }
}

impl Plugin for HistorySearch {
    fn name(&self) -> &'static str {
        "history-search"
    }
    fn priority(&self) -> i32 {
        100
    }
    fn bindings(&self) -> &'static [(Key, &'static str)] {
        &[(Key::Ctrl(b'r'), "history-search")]
    }
    fn on_action(&mut self, _ctx: &mut Ctx, action: &str) -> Handled {
        if action != "history-search" {
            return Handled::Pass;
        }
        self.st = Some(State {
            query: String::new(),
            pool: None,
            results: Vec::new(),
            sel: 0,
        });
        self.requery(false);
        Handled::Yes
    }
    fn on_key(&mut self, ctx: &mut Ctx, key: &Key) -> Handled {
        let Some(s) = &mut self.st else {
            return Handled::Pass;
        };
        let mut requery = None;
        match key {
            Key::Char(c) => {
                s.query.push(*c);
                requery = Some(true);
            }
            Key::Paste(t) => {
                s.query.push_str(t.lines().next().unwrap_or(""));
                requery = Some(true);
            }
            Key::Backspace => {
                s.query.pop();
                requery = Some(false);
            }
            Key::Ctrl(b'u') => {
                s.query.clear();
                requery = Some(false);
            }
            Key::Ctrl(b'w') => {
                let t = s.query.trim_end().len();
                let cut = s.query[..t].rfind(' ').map_or(0, |i| i + 1);
                s.query.truncate(cut);
                requery = Some(false);
            }
            Key::Ctrl(b'r') | Key::Down | Key::Ctrl(b'n') => {
                if s.sel + 1 < s.results.len() {
                    s.sel += 1;
                }
            }
            Key::Up | Key::Ctrl(b'p') | Key::Ctrl(b's') => s.sel = s.sel.saturating_sub(1),
            Key::Enter | Key::Tab | Key::Right | Key::End => {
                let pick = s.results.get(s.sel).copied();
                self.st = None;
                if let Some(text) = pick.and_then(|i| self.hist.with(|h| h.get(i).into_owned())) {
                    ctx.set_text(&text);
                }
                return Handled::Yes;
            }
            Key::Esc | Key::Ctrl(b'g') | Key::Ctrl(b'c') => {
                self.st = None;
                return Handled::Yes;
            }
            _ => {}
        }
        if let Some(narrow) = requery {
            self.requery(narrow);
        }
        Handled::Yes
    }
    fn decorate(&mut self, _ctx: &mut Ctx, _width: u16, out: &mut Decoration) {
        let Some(s) = &self.st else { return };
        out.show_input(&s.query, s.query.len());
        out.hide_ghost();
        let ci = !s.query.bytes().any(|b| b.is_ascii_uppercase());
        for (row, &i) in s.results.iter().enumerate() {
            let sel = row == s.sel;
            let Some(cand) = self.hist.with(|h| h.get(i).into_owned()) else {
                continue;
            };
            fuzzy(s.query.as_bytes(), cand.as_bytes(), ci, Some(&mut self.pos));
            let pos = &self.pos;
            out.push_row(|r| {
                let base = if sel {
                    RowStyle::Selected
                } else {
                    RowStyle::Normal
                };
                let hl = if sel {
                    RowStyle::MatchedSelected
                } else {
                    RowStyle::Matched
                };
                r.push(base, if sel { "> " } else { "  " });
                let mut pi = 0;
                for (bi, c) in cand.char_indices() {
                    while pi < pos.len() && pos[pi] < bi {
                        pi += 1;
                    }
                    let m = pi < pos.len() && pos[pi] < bi + c.len_utf8();
                    let mut tmp = [0u8; 4];
                    r.push(if m { hl } else { base }, c.encode_utf8(&mut tmp));
                }
            });
        }
    }
}
