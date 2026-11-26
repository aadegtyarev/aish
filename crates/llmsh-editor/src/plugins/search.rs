use std::io::Write;

use unicode_width::UnicodeWidthChar;

use super::history::SharedHistory;
use crate::history::fuzzy;
use crate::keys::Key;
use crate::plugin::{Ctx, Decoration, Handled, Plugin};

const ROWS: usize = 8;

struct State {
    query: String,
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

    fn requery(&mut self) {
        if let Some(s) = &mut self.st {
            let q = s.query.clone();
            self.hist.with(|h| h.search(&q, ROWS, &mut s.results));
            s.sel = 0;
        }
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
            results: Vec::new(),
            sel: 0,
        });
        self.requery();
        Handled::Yes
    }
    fn on_key(&mut self, ctx: &mut Ctx, key: &Key) -> Handled {
        let Some(s) = &mut self.st else {
            return Handled::Pass;
        };
        let mut requery = false;
        match key {
            Key::Char(c) => {
                s.query.push(*c);
                requery = true;
            }
            Key::Paste(t) => {
                s.query.push_str(t.lines().next().unwrap_or(""));
                requery = true;
            }
            Key::Backspace => {
                s.query.pop();
                requery = true;
            }
            Key::Ctrl(b'u') => {
                s.query.clear();
                requery = true;
            }
            Key::Ctrl(b'w') => {
                let t = s.query.trim_end().len();
                let cut = s.query[..t].rfind(' ').map_or(0, |i| i + 1);
                s.query.truncate(cut);
                requery = true;
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
                if let Some(text) = pick.and_then(|i| self.hist.with(|h| h.get(i).to_owned())) {
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
        if requery {
            self.requery();
        }
        Handled::Yes
    }
    fn decorate(&mut self, ctx: &mut Ctx, width: u16, out: &mut Decoration) {
        let Some(s) = &self.st else { return };
        out.show_input(&s.query, s.query.len());
        out.hide_ghost();
        let ci = !s.query.bytes().any(|b| b.is_ascii_uppercase());
        let th = ctx.theme().clone();
        for (row, &i) in s.results.iter().enumerate() {
            let sel = row == s.sel;
            let Some(cand) = self.hist.with(|h| h.get(i).to_owned()) else {
                continue;
            };
            fuzzy(s.query.as_bytes(), cand.as_bytes(), ci, Some(&mut self.pos));
            let pos = &self.pos;
            out.push_row(|r| {
                let _ = if sel {
                    write!(r, "\x1b[{}m> ", th.selection)
                } else {
                    write!(r, "  ")
                };
                let mut budget = usize::from(width).saturating_sub(4);
                let mut hl = false;
                let mut pi = 0;
                for (bi, c) in cand.char_indices() {
                    let w = if c < ' ' { 1 } else { c.width().unwrap_or(0) };
                    if w > budget {
                        break;
                    }
                    budget -= w;
                    while pi < pos.len() && pos[pi] < bi {
                        pi += 1;
                    }
                    let m = pi < pos.len() && pos[pi] < bi + c.len_utf8();
                    if m != hl {
                        hl = m;
                        let _ = write!(r, "\x1b[0m");
                        if sel {
                            let _ = write!(r, "\x1b[{}m", th.selection);
                        }
                        if m {
                            let _ = write!(r, "\x1b[{}m", th.matched);
                        }
                    }
                    let c = if c < ' ' { ' ' } else { c };
                    let mut tmp = [0u8; 4];
                    r.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                }
                let _ = write!(r, "\x1b[0m\x1b[K");
            });
        }
    }
}
