use std::any::Any;
use std::io::Write;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::completion::{self, Candidates, CompletionSource, Outcome, Reply, Request};
use crate::keys::Key;
use crate::plugin::{Ctx, Decoration, Handled, Plugin};

const ROWS: usize = 8;

struct Pager {
    items: Vec<String>,
    sel: Option<usize>,
    orig_text: String,
    orig_cursor: usize,
    start: usize,
    cur: String,
    filenames: bool,
    nospace: bool,
    width: u16,
    own_rev: u64,
}

/// Tab completion UI (action `complete`) over any [`CompletionSource`]:
/// a single candidate is inserted, a common prefix is inserted, otherwise a
/// column pager (max 8 rows) opens below the input. Tab / Shift-Tab / arrows
/// move with a live preview in the line, Enter or a printable key accepts,
/// Esc reverts.
pub struct CompletionPager {
    source: Box<dyn CompletionSource>,
    pager: Option<Pager>,
    pending: Option<Request>,
}

impl CompletionPager {
    /// Complete with `source`.
    pub fn new(source: impl CompletionSource + 'static) -> Self {
        CompletionPager {
            source: Box::new(source),
            pager: None,
            pending: None,
        }
    }

    fn apply(&mut self, ctx: &mut Ctx, req: &Request, cands: Candidates) {
        let Candidates {
            filenames,
            nospace,
            items,
        } = cands;
        let cur = req.cur();
        let end = ctx.cursor();
        let start = req.word_start;
        match items.len() {
            0 => {}
            1 => {
                let mut ins = completion::quote(&items[0], cur, filenames);
                if !(ins.ends_with('/') || nospace) {
                    ins.push(' ');
                }
                ctx.replace_range(start, end, &ins);
            }
            _ => {
                let typed = completion::unescape(cur);
                let lcp = completion::common_prefix(&items);
                if lcp.len() > typed.len() {
                    let ins = completion::quote(lcp, cur, filenames);
                    ctx.replace_range(start, end, &ins);
                } else {
                    self.pager = Some(Pager {
                        items,
                        sel: None,
                        orig_text: ctx.text().to_owned(),
                        orig_cursor: end,
                        start,
                        cur: cur.to_owned(),
                        filenames,
                        nospace,
                        width: 80,
                        own_rev: ctx.generation(),
                    });
                }
            }
        }
    }
}

fn label(p: &Pager, i: usize) -> &str {
    let it = p.items[i].as_str();
    if p.filenames {
        let t = it.trim_end_matches('/');
        let base = t.rfind('/').map_or(0, |x| x + 1);
        &it[base..]
    } else {
        it
    }
}

fn maxw(p: &Pager) -> usize {
    let width = usize::from(p.width).max(8);
    (0..p.items.len())
        .map(|i| label(p, i).width())
        .max()
        .unwrap_or(1)
        .min(width.saturating_sub(4))
}

/// (columns, rows on this page, items per page)
fn dims(p: &Pager) -> (usize, usize, usize) {
    let width = usize::from(p.width).max(8);
    let colw = maxw(p) + 2;
    let ncols = ((width - 1) / colw).max(1);
    let cap = ncols * ROWS;
    let page = p.sel.unwrap_or(0) / cap;
    let m = cap.min(p.items.len() - page * cap);
    (ncols, m.div_ceil(ncols), cap)
}

impl Plugin for CompletionPager {
    fn name(&self) -> &'static str {
        "completion-pager"
    }
    fn priority(&self) -> i32 {
        100
    }
    fn bindings(&self) -> &'static [(Key, &'static str)] {
        &[(Key::Tab, "complete")]
    }

    fn on_action(&mut self, ctx: &mut Ctx, action: &str) -> Handled {
        if action != "complete" {
            return Handled::Pass;
        }
        if ctx.text().is_empty() {
            return Handled::Yes;
        }
        let req = completion::parse(ctx.text(), ctx.cursor());
        let reply = Reply {
            handle: ctx.handle(),
            generation: ctx.generation(),
        };
        match self.source.complete(&req, reply) {
            Outcome::Ready(c) => self.apply(ctx, &req, c),
            Outcome::Pending => self.pending = Some(req),
            Outcome::None => {}
        }
        Handled::Yes
    }

    fn on_delivery(&mut self, ctx: &mut Ctx, payload: Box<dyn Any + Send>) {
        if let (Ok(c), Some(req)) = (payload.downcast::<Candidates>(), self.pending.take()) {
            self.apply(ctx, &req, *c);
            ctx.request_redraw();
        }
    }

    fn on_buffer_change(&mut self, ctx: &mut Ctx) {
        if self
            .pager
            .as_ref()
            .is_some_and(|p| p.own_rev != ctx.generation())
        {
            self.pager = None;
        }
    }

    fn on_key(&mut self, ctx: &mut Ctx, key: &Key) -> Handled {
        let Some(p) = &mut self.pager else {
            return Handled::Pass;
        };
        let n = p.items.len();
        let (_, nrows, cap) = dims(p);
        let step = |sel: Option<usize>, d: isize| -> usize {
            match sel {
                None if d > 0 => 0,
                None => n - 1,
                Some(i) => (i as isize + d).rem_euclid(n as isize) as usize,
            }
        };
        let page_move = |sel: Option<usize>, cols: isize| -> usize {
            let i = sel.unwrap_or(0);
            let page = i / cap;
            let k = (i % cap) as isize + cols * nrows as isize;
            let m = cap.min(n - page * cap) as isize;
            if (0..m).contains(&k) {
                page * cap + k as usize
            } else {
                i
            }
        };
        match key {
            Key::Tab | Key::Down | Key::Ctrl(b'n') => p.sel = Some(step(p.sel, 1)),
            Key::BackTab | Key::Up | Key::Ctrl(b'p') => p.sel = Some(step(p.sel, -1)),
            Key::Right => p.sel = Some(page_move(p.sel, 1)),
            Key::Left => p.sel = Some(page_move(p.sel, -1)),
            Key::Esc => {
                ctx.set_text(&p.orig_text);
                ctx.set_cursor(p.orig_cursor);
                self.pager = None;
                return Handled::Yes;
            }
            Key::Enter if p.sel.is_some() => {
                let i = p.sel.unwrap_or(0);
                if !(p.items[i].ends_with('/') || p.nospace) {
                    ctx.insert(" ");
                }
                self.pager = None;
                return Handled::Yes;
            }
            _ => {
                // Keep what is previewed and let the key act normally.
                self.pager = None;
                return Handled::Pass;
            }
        }
        if let Some(i) = p.sel {
            let ins = completion::quote(&p.items[i], &p.cur, p.filenames);
            let mut s = String::with_capacity(p.orig_text.len() + ins.len());
            s.push_str(&p.orig_text[..p.start]);
            s.push_str(&ins);
            s.push_str(&p.orig_text[p.orig_cursor..]);
            ctx.set_text(&s);
            ctx.set_cursor(p.start + ins.len());
            p.own_rev = ctx.generation();
        }
        Handled::Yes
    }

    fn decorate(&mut self, ctx: &mut Ctx, width: u16, out: &mut Decoration) {
        let Some(p) = &mut self.pager else { return };
        p.width = width;
        let th = ctx.theme().clone();
        let n = p.items.len();
        let (ncols, nrows, cap) = dims(p);
        let mw = maxw(p);
        let colw = mw + 2;
        let base = p.sel.unwrap_or(0) / cap * cap;
        let m = cap.min(n - base);
        let p = &*p;
        for r in 0..nrows {
            out.push_row(|o| {
                for c in 0..ncols {
                    let k = r + c * nrows;
                    if k >= m {
                        break;
                    }
                    let selected = p.sel == Some(base + k);
                    if selected {
                        let _ = write!(o, "\x1b[{}m", th.selection);
                    }
                    let mut used = 0;
                    for ch in label(p, base + k).chars() {
                        let w = ch.width().unwrap_or(0);
                        if used + w > mw {
                            break;
                        }
                        used += w;
                        let mut tmp = [0u8; 4];
                        o.extend_from_slice(
                            if ch < ' ' {
                                " "
                            } else {
                                ch.encode_utf8(&mut tmp)
                            }
                            .as_bytes(),
                        );
                    }
                    if selected {
                        o.extend_from_slice(b"\x1b[0m");
                    }
                    o.resize(o.len() + colw - used, b' ');
                }
            });
        }
        if base + m < n {
            out.push_row(|o| {
                let _ = write!(o, "\x1b[{}m\u{2026} {} more\x1b[0m", th.dim, n - base - m);
            });
        }
    }
}
