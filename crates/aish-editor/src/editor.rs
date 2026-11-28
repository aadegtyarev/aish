//! The editor core: basic editing, key dispatch, plugin registry, drawing.
//!
//! No terminal, shell, process or OSC knowledge lives here. The host passes
//! bytes from the keyboard to [`Editor::input`], draws with [`Editor::draw`]
//! (terminal width and the column where the input starts are given by the
//! host) and reacts to the returned [`Event`]s.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::buffer::Buffer;
use crate::highlight::{Class, Span};
use crate::keymap::{Binding, Command, Keymap};
use crate::keys::{Decoder, Key, One};
use crate::plugin::{Ctx, Decoration, Delivery, Handle, Handled, Plugin, RowStyle, SubmitAction};
use crate::render::{Renderer, View};
use crate::theme::Theme;

/// What the host has to do after feeding input.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A finished line.
    Submit(String),
    /// A line a plugin claimed ([`SubmitAction::Intercept`]); the host handles
    /// it instead of treating it as a normal line.
    Intercepted(String),
    /// Bytes for the program under the editor: Ctrl-C (`\x03`, line already
    /// cleared), Ctrl-D (`\x04`, empty line), Ctrl-L (`\x0c`) and keys typed
    /// after a line ended.
    PassKey(Vec<u8>),
    /// Reply to the host's cursor-position query: (row, col), 1-based.
    CursorReport(u16, u16),
    /// State changed outside of input (a late result): redraw.
    NeedRedraw,
}

/// Internal result of one key.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    None,
    Submit,
    Interrupt,
    Eof,
    ClearScreen,
}

/// Builds an [`Editor`].
pub struct EditorBuilder {
    theme: Theme,
    keymap: Keymap,
    plugins: Vec<Box<dyn Plugin>>,
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl EditorBuilder {
    /// Colors and attributes (default [`Theme::from_env`]).
    pub fn theme(mut self, t: Theme) -> Self {
        self.theme = t;
        self
    }
    /// Key bindings (default [`Keymap::emacs`]).
    pub fn keymap(mut self, k: Keymap) -> Self {
        self.keymap = k;
        self
    }
    /// Register a plugin.
    pub fn with(mut self, p: impl Plugin + 'static) -> Self {
        self.plugins.push(Box::new(p));
        self
    }
    /// Register an already boxed plugin.
    pub fn with_boxed(mut self, p: Box<dyn Plugin>) -> Self {
        self.plugins.push(p);
        self
    }
    /// Called (from any thread) when a plugin delivered something: the host
    /// should wake its loop and call [`Editor::poll`].
    pub fn waker(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.waker = Some(Arc::new(f));
        self
    }
    /// Finish.
    pub fn build(mut self) -> Editor {
        // Stable: equal priorities keep registration order.
        self.plugins
            .sort_by_key(|p| std::cmp::Reverse(p.priority()));
        for p in &self.plugins {
            for (key, action) in p.bindings() {
                if self.keymap.get(key).is_none() {
                    self.keymap.bind_action(key.clone(), action);
                }
            }
        }
        Editor {
            buf: Buffer::new(),
            theme: self.theme,
            keymap: self.keymap,
            plugins: self.plugins,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            waker: self.waker,
            rend: Renderer::default(),
            dec: Decoder::new(),
            scratch: Vec::with_capacity(4096),
            ghost_buf: String::with_capacity(128),
            spans: Vec::with_capacity(32),
            tmp_spans: Vec::with_capacity(32),
            cls: Vec::with_capacity(256),
            deco: Decoration::default(),
            deco_rows: Vec::with_capacity(512),
            deco_nrows: 0,
            last_rev: 0,
        }
    }
}

/// The line editor. See the crate docs for the driving loop.
pub struct Editor {
    buf: Buffer,
    theme: Theme,
    keymap: Keymap,
    plugins: Vec<Box<dyn Plugin>>,
    queue: Arc<Mutex<VecDeque<Delivery>>>,
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
    rend: Renderer,
    dec: Decoder,
    scratch: Vec<u8>,
    ghost_buf: String,
    spans: Vec<Span>,
    tmp_spans: Vec<Span>,
    cls: Vec<Option<Class>>,
    deco: Decoration,
    deco_rows: Vec<u8>,
    deco_nrows: u16,
    last_rev: u64,
}

fn make_ctx<'a>(
    buf: &'a mut Buffer,
    theme: &'a Theme,
    queue: &Arc<Mutex<VecDeque<Delivery>>>,
    waker: &Option<Arc<dyn Fn() + Send + Sync>>,
    plugin: usize,
) -> Ctx<'a> {
    Ctx {
        buf,
        theme,
        handle: Handle {
            queue: queue.clone(),
            plugin,
            waker: waker.clone(),
        },
        redraw: false,
    }
}

impl Editor {
    /// Start building an editor.
    pub fn builder() -> EditorBuilder {
        EditorBuilder {
            theme: Theme::from_env(),
            keymap: Keymap::emacs(),
            plugins: Vec::new(),
            waker: None,
        }
    }

    /// Run `f` for every plugin in priority order with a context; `f` returns
    /// true to stop.
    fn each_plugin(&mut self, mut f: impl FnMut(&mut dyn Plugin, &mut Ctx) -> bool) -> bool {
        let mut plugins = std::mem::take(&mut self.plugins);
        let mut stopped = false;
        for (i, p) in plugins.iter_mut().enumerate() {
            let mut ctx = make_ctx(&mut self.buf, &self.theme, &self.queue, &self.waker, i);
            if f(&mut **p, &mut ctx) {
                stopped = true;
                break;
            }
        }
        self.plugins = plugins;
        stopped
    }

    fn after_change(&mut self) {
        if self.buf.rev() != self.last_rev {
            self.each_plugin(|p, c| {
                p.on_buffer_change(c);
                false
            });
            self.last_rev = self.buf.rev();
        }
    }

    /// Fill `ghost_buf` from the first plugin that has a suggestion.
    fn compute_ghost(&mut self) {
        self.ghost_buf.clear();
        if !self.theme.enabled || !self.buf.at_end() || self.buf.is_empty() {
            return;
        }
        let mut plugins = std::mem::take(&mut self.plugins);
        let mut ghost = std::mem::take(&mut self.ghost_buf);
        for (i, p) in plugins.iter_mut().enumerate() {
            let mut ctx = make_ctx(&mut self.buf, &self.theme, &self.queue, &self.waker, i);
            if let Some(s) = p.suggest(&mut ctx) {
                let t = ctx.text();
                if s.len() > t.len() && s.starts_with(t) {
                    ghost.push_str(&s[t.len()..]);
                    break;
                }
            }
        }
        self.ghost_buf = ghost;
        self.plugins = plugins;
    }

    fn accept_ghost(&mut self, word: bool) -> bool {
        self.compute_ghost();
        if self.ghost_buf.is_empty() {
            return false;
        }
        let g = std::mem::take(&mut self.ghost_buf);
        let n = if word {
            Buffer::word_right_pos_in(&g).max(1)
        } else {
            g.len()
        };
        self.buf.insert_str(&g[..n]);
        self.ghost_buf = g;
        true
    }

    /// Take the submitted line through the plugins' `on_submit` hooks.
    fn finish_line(&mut self) -> Event {
        let mut line = self.buf.take();
        let mut intercepted = false;
        let mut plugins = std::mem::take(&mut self.plugins);
        for (i, p) in plugins.iter_mut().enumerate() {
            let mut ctx = make_ctx(&mut self.buf, &self.theme, &self.queue, &self.waker, i);
            match p.on_submit(&mut ctx, &line) {
                SubmitAction::Submit => {}
                SubmitAction::Intercept => intercepted = true,
                SubmitAction::Edit(l) => line = l,
            }
        }
        self.plugins = plugins;
        self.buf.clear();
        self.last_rev = self.buf.rev();
        if intercepted {
            Event::Intercepted(line)
        } else {
            Event::Submit(line)
        }
    }

    pub(crate) fn handle_key(&mut self, key: Key) -> Action {
        let act = self.dispatch(key);
        self.after_change();
        act
    }

    fn dispatch(&mut self, key: Key) -> Action {
        let mut submit = false;
        if self.each_plugin(|p, c| {
            let h = p.on_key(c, &key);
            submit = h == Handled::Submit;
            h != Handled::Pass
        }) {
            return if submit { Action::Submit } else { Action::None };
        }
        if let Key::Paste(t) = &key {
            let t = t.replace("\r\n", "\n").replace('\r', "\n");
            let t: String = t.chars().filter(|&c| c != '\x1b').collect();
            self.buf.insert_str(&t);
            return Action::None;
        }
        match self.keymap.get(&key).cloned() {
            Some(Binding::Command(c)) => self.exec(c),
            Some(Binding::Action(name)) => {
                self.each_plugin(|p, c| p.on_action(c, &name) == Handled::Yes);
                Action::None
            }
            None => {
                if let Key::Char(c) = key {
                    self.buf.insert(c);
                }
                Action::None
            }
        }
    }

    fn exec(&mut self, cmd: Command) -> Action {
        use Command::*;
        match cmd {
            Accept => return Action::Submit,
            CharLeft => self.buf.left(),
            CharRight => {
                if !self.accept_ghost(false) {
                    self.buf.right();
                }
            }
            LineEnd => {
                if !self.accept_ghost(false) {
                    self.buf.end();
                }
            }
            WordRight => {
                if !self.accept_ghost(true) {
                    self.buf.word_right();
                }
            }
            WordLeft => self.buf.word_left(),
            LineStart => self.buf.home(),
            DeleteBack => self.buf.backspace(),
            DeleteForward => self.buf.delete(),
            DeleteOrEof => {
                if self.buf.is_empty() {
                    return Action::Eof;
                }
                self.buf.delete();
            }
            KillWordBackWs => self.buf.kill_ws_word_back(),
            KillWordBack => self.buf.kill_word_back(),
            KillWordForward => self.buf.kill_word_fwd(),
            KillToStart => self.buf.kill_to_start(),
            KillToEnd => self.buf.kill_to_end(),
            Yank => self.buf.yank(),
            Undo => self.buf.undo(),
            Transpose => self.buf.transpose(),
            Interrupt => {
                self.buf.clear();
                return Action::Interrupt;
            }
            ClearScreen => return Action::ClearScreen,
        }
        Action::None
    }

    /// Feed bytes read from the keyboard. Events for the host are appended to
    /// `events`. After a terminal event (`Submit`, `Intercepted`, `PassKey`
    /// from Ctrl-C/D/L) the rest of the chunk is returned unchanged as a
    /// trailing `PassKey` (including any partial key), so the host can hand it
    /// to whatever comes next.
    pub fn input(&mut self, bytes: &[u8], events: &mut Vec<Event>) {
        let carried;
        let data: &[u8] = if self.dec.has_pending() {
            let mut v = self.dec.take_pending();
            v.extend_from_slice(bytes);
            carried = v;
            &carried
        } else {
            bytes
        };
        let mut i = 0;
        while i < data.len() {
            match self.dec.parse_one(&data[i..]) {
                One::Skip(n) => i += n,
                One::Incomplete => {
                    self.dec.set_pending(&data[i..]);
                    return;
                }
                One::Key(k, n) => {
                    i += n;
                    if self.on_key_event(k, events) {
                        if i < data.len() {
                            events.push(Event::PassKey(data[i..].to_vec()));
                        }
                        return;
                    }
                }
            }
        }
    }

    /// True when a partial key (typically a lone ESC) is waiting: call
    /// [`Editor::flush_input`] if nothing arrives for ~30 ms.
    pub fn has_pending_input(&self) -> bool {
        self.dec.has_pending()
    }

    /// The pending partial key was a real key press (Escape).
    pub fn flush_input(&mut self, events: &mut Vec<Event>) {
        if let Some(k) = self.dec.flush() {
            self.on_key_event(k, events);
        }
    }

    /// Handle one key; true if it produced a terminal event.
    fn on_key_event(&mut self, k: Key, events: &mut Vec<Event>) -> bool {
        if let Key::CursorPos(r, c) = k {
            events.push(Event::CursorReport(r, c));
            return false;
        }
        let ev = match self.handle_key(k) {
            Action::None => return false,
            Action::Submit => self.finish_line(),
            Action::Interrupt => Event::PassKey(vec![0x03]),
            Action::Eof => Event::PassKey(vec![0x04]),
            Action::ClearScreen => Event::PassKey(vec![0x0c]),
        };
        events.push(ev);
        true
    }

    /// Process results plugins delivered through their handles. Appends
    /// [`Event::NeedRedraw`] if something visible may have changed.
    pub fn poll(&mut self, events: &mut Vec<Event>) {
        let items: Vec<Delivery> = match self.queue.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => return,
        };
        let mut redraw = false;
        for d in items {
            if (d.generation != self.buf.rev() && d.generation != crate::plugin::ANY_GENERATION)
                || d.plugin >= self.plugins.len()
            {
                continue; // stale: the line changed since the work started
            }
            let mut plugins = std::mem::take(&mut self.plugins);
            let mut ctx = make_ctx(
                &mut self.buf,
                &self.theme,
                &self.queue,
                &self.waker,
                d.plugin,
            );
            plugins[d.plugin].on_delivery(&mut ctx, d.payload);
            self.plugins = plugins;
            redraw = true;
        }
        self.after_change();
        if redraw {
            events.push(Event::NeedRedraw);
        }
    }

    /// Current line text.
    pub fn text(&self) -> &str {
        self.buf.text()
    }

    /// Cursor position as a byte offset into [`Editor::text`].
    pub fn cursor(&self) -> usize {
        self.buf.cursor()
    }

    /// Replace the line (cursor to the end).
    pub fn set_text(&mut self, s: &str) {
        self.buf.set(s);
        self.after_change();
    }

    /// Clear the line.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.after_change();
    }

    /// The theme in use.
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Tell the editor in which terminal column (0-based) the input area
    /// starts, and forget what was drawn before (nothing is erased).
    pub fn reset_screen(&mut self, col: u16) {
        self.rend.reset(col);
    }

    /// Like [`reset_screen`](Editor::reset_screen) for a prompt whose last
    /// line the terminal wrapped over `wrapped` full rows of `width` columns.
    pub fn reset_screen_wrapped(&mut self, col: u16, wrapped: u16, width: u16) {
        self.rend.reset_wrapped(col, wrapped, width);
    }

    /// Hide the right-edge hint (see `Renderer::no_hint`); cleared by a reset.
    pub fn hide_hint(&mut self, hide: bool) {
        self.rend.no_hint = hide;
    }

    /// Column where the input area starts.
    pub fn start_col(&self) -> u16 {
        self.rend.start_col
    }

    fn prepare(&mut self, width: u16) {
        // highlight layers: low priority first so higher ones win
        self.spans.clear();
        if self.theme.enabled {
            let text = self.buf.text();
            let mut any = false;
            self.cls.clear();
            self.cls.resize(text.len(), None);
            for p in self.plugins.iter().rev() {
                self.tmp_spans.clear();
                p.highlight(text, &mut self.tmp_spans);
                for s in &self.tmp_spans {
                    let e = s.end.min(text.len());
                    if s.start < e {
                        any = true;
                        self.cls[s.start..e].fill(Some(s.class));
                    }
                }
            }
            if any {
                let mut i = 0;
                while i < self.cls.len() {
                    match self.cls[i] {
                        Some(c) if c != Class::Default => {
                            let st = i;
                            while i < self.cls.len() && self.cls[i] == Some(c) {
                                i += 1;
                            }
                            self.spans.push(Span {
                                start: st,
                                end: i,
                                class: c,
                            });
                        }
                        _ => i += 1,
                    }
                }
            }
        }
        // decorations
        self.deco = Decoration::default();
        let mut deco = std::mem::take(&mut self.deco);
        self.each_plugin(|p, c| {
            p.decorate(c, width, &mut deco);
            false
        });
        self.deco = deco;
        self.render_rows(width);
        if self.deco.hide_ghost {
            self.ghost_buf.clear();
        } else {
            self.compute_ghost();
        }
    }

    /// Turn decoration rows into bytes: theme styles, control characters
    /// replaced, truncated to the terminal width.
    fn render_rows(&mut self, width: u16) {
        use unicode_width::UnicodeWidthChar;
        self.deco_rows.clear();
        self.deco_nrows = 0;
        let th = &self.theme;
        for (n, row) in self.deco.lines.iter().enumerate() {
            if n > 0 {
                self.deco_rows.push(b'\n');
            }
            let mut budget = usize::from(width).saturating_sub(1);
            'row: for (style, text) in &row.segs {
                let sgr = match style {
                    RowStyle::Normal => String::new(),
                    RowStyle::Selected => th.selection.clone(),
                    RowStyle::Matched => th.matched.clone(),
                    RowStyle::MatchedSelected => format!("{};{}", th.selection, th.matched),
                    RowStyle::Dim => th.dim.clone(),
                };
                let _ = write!(self.deco_rows, "\x1b[0;{sgr}m");
                for c in text.chars() {
                    let c = if c.is_control() { ' ' } else { c };
                    let w = c.width().unwrap_or(0);
                    if w > budget {
                        break 'row;
                    }
                    budget -= w;
                    let mut tmp = [0u8; 4];
                    self.deco_rows
                        .extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                }
            }
            self.deco_rows.extend_from_slice(b"\x1b[0m\x1b[K");
            self.deco_nrows += 1;
        }
    }

    fn view(&self) -> View<'_> {
        let (text, cursor) = match &self.deco.input {
            Some((t, c)) => (t.as_str(), *c),
            None => (self.buf.text(), self.buf.cursor()),
        };
        View {
            text,
            cursor,
            spans: if self.deco.input.is_some() {
                &[]
            } else {
                &self.spans
            },
            ghost: &self.ghost_buf,
            extra: &self.deco_rows,
            extra_rows: self.deco_nrows,
            right: if self.deco.input.is_some() {
                None
            } else {
                self.deco.right.as_deref()
            },
            theme: &self.theme,
        }
    }

    /// Draw the input area (text, suggestion, popups) for a terminal of
    /// `width` columns as one synchronized-output write.
    pub fn draw(&mut self, out: &mut dyn Write, width: u16) -> std::io::Result<()> {
        self.prepare(width);
        let mut rend = std::mem::take(&mut self.rend);
        let mut buf = std::mem::take(&mut self.scratch);
        buf.clear();
        rend.draw(&mut buf, &self.view(), width);
        let r = out.write_all(&buf);
        self.rend = rend;
        self.scratch = buf;
        r
    }

    /// Erase the input area; the cursor returns to where the input starts.
    pub fn erase(&mut self, out: &mut dyn Write) -> std::io::Result<()> {
        let mut buf = std::mem::take(&mut self.scratch);
        buf.clear();
        self.rend.erase(&mut buf);
        let r = out.write_all(&buf);
        self.scratch = buf;
        r
    }

    /// Erase after the terminal was resized to `width` columns (assumes the
    /// terminal reflowed what we had drawn).
    ///
    /// Returns true when the cursor was left at the top of the prompt (always,
    /// for now): the shell is expected to repaint it, and drawing
    /// before that would land on the wrong row.
    pub fn erase_resized(&mut self, out: &mut dyn Write, width: u16) -> std::io::Result<bool> {
        let mut buf = std::mem::take(&mut self.scratch);
        buf.clear();
        let top = self.rend.erase_resized(&mut buf, width);
        let r = out.write_all(&buf);
        self.scratch = buf;
        r.map(|()| top)
    }

    /// After a [`draw`](Editor::draw): the column where the input (with
    /// suggestion) ends if it all fits on the first row and no popup is shown;
    /// hosts use it to keep a right prompt visible.
    pub fn first_row_end(&self) -> Option<u16> {
        self.rend.first_row_end
    }

    /// Draw `line` as final text (no suggestion or popup) with the cursor at
    /// its end; used to leave an intercepted line on screen.
    pub fn draw_final(
        &mut self,
        out: &mut dyn Write,
        width: u16,
        line: &str,
    ) -> std::io::Result<()> {
        self.ghost_buf.clear();
        self.deco = Decoration::default();
        self.deco_rows.clear();
        self.deco_nrows = 0;
        self.spans.clear();
        let mut rend = std::mem::take(&mut self.rend);
        let mut buf = std::mem::take(&mut self.scratch);
        buf.clear();
        let mut v = self.view();
        v.text = line;
        v.cursor = line.len();
        v.extra = b"";
        v.extra_rows = 0;
        v.right = None;
        rend.draw(&mut buf, &v, width);
        let r = out.write_all(&buf);
        self.rend = rend;
        self.scratch = buf;
        r
    }
}
