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
use crate::keys::{Decoder, Key};
use crate::plugin::{Ctx, Decoration, Delivery, Handle, Handled, Plugin, SubmitAction};
use crate::render::{Renderer, View};
use crate::theme::Theme;

/// What the host has to do after feeding input.
#[derive(Debug, PartialEq, Eq)]
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
            keys: Vec::with_capacity(16),
            scratch: Vec::with_capacity(4096),
            ghost_buf: String::with_capacity(128),
            spans: Vec::with_capacity(32),
            tmp_spans: Vec::with_capacity(32),
            cls: Vec::with_capacity(256),
            deco: Decoration::default(),
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
    keys: Vec<Key>,
    scratch: Vec<u8>,
    ghost_buf: String,
    spans: Vec<Span>,
    tmp_spans: Vec<Span>,
    cls: Vec<Option<Class>>,
    deco: Decoration,
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
        if self.each_plugin(|p, c| p.on_key(c, &key) == Handled::Yes) {
            return Action::None;
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

    // ---- driving API ----

    /// Feed bytes read from the keyboard. Events for the host are appended to
    /// `events`; after a terminal event (`Submit`, `Intercepted`, `PassKey`
    /// from Ctrl-C/D/L) the rest of the chunk is returned as a trailing `PassKey`.
    pub fn input(&mut self, bytes: &[u8], events: &mut Vec<Event>) {
        let mut keys = std::mem::take(&mut self.keys);
        let mut dec = std::mem::replace(&mut self.dec, Decoder::new());
        dec.feed(bytes, &mut |k| keys.push(k));
        self.dec = dec;
        self.run_keys(&mut keys, events);
        self.keys = keys;
    }

    /// True when a partial key (typically a lone ESC) is waiting: call
    /// [`Editor::flush_input`] if nothing arrives for ~30 ms.
    pub fn has_pending_input(&self) -> bool {
        self.dec.has_pending()
    }

    /// The pending partial key was a real key press (Escape).
    pub fn flush_input(&mut self, events: &mut Vec<Event>) {
        let mut keys = std::mem::take(&mut self.keys);
        let mut dec = std::mem::replace(&mut self.dec, Decoder::new());
        dec.flush(&mut |k| keys.push(k));
        self.dec = dec;
        self.run_keys(&mut keys, events);
        self.keys = keys;
    }

    fn run_keys(&mut self, keys: &mut Vec<Key>, events: &mut Vec<Event>) {
        let mut it = keys.drain(..);
        while let Some(k) = it.next() {
            if let Key::CursorPos(r, c) = k {
                events.push(Event::CursorReport(r, c));
                continue;
            }
            let ev = match self.handle_key(k) {
                Action::None => continue,
                Action::Submit => self.finish_line(),
                Action::Interrupt => Event::PassKey(vec![0x03]),
                Action::Eof => Event::PassKey(vec![0x04]),
                Action::ClearScreen => Event::PassKey(vec![0x0c]),
            };
            events.push(ev);
            let rest: Vec<Key> = it.collect();
            let b = encode_keys(&rest);
            if !b.is_empty() {
                events.push(Event::PassKey(b));
            }
            return;
        }
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
            if d.generation != self.buf.rev() || d.plugin >= self.plugins.len() {
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
        if self.deco.hide_ghost {
            self.ghost_buf.clear();
        } else {
            self.compute_ghost();
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
            extra: &self.deco.rows,
            extra_rows: self.deco.nrows,
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
        self.spans.clear();
        let mut rend = std::mem::take(&mut self.rend);
        let mut buf = std::mem::take(&mut self.scratch);
        buf.clear();
        let mut v = self.view();
        v.text = line;
        v.cursor = line.len();
        rend.draw(&mut buf, &v, width);
        let r = out.write_all(&buf);
        self.rend = rend;
        self.scratch = buf;
        r
    }
}

/// Re-encode keys as bytes for pass-through (best effort).
fn encode_keys(keys: &[Key]) -> Vec<u8> {
    let mut b = Vec::new();
    for k in keys {
        match k {
            Key::Char(c) => b.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
            Key::Enter => b.push(b'\r'),
            Key::Tab => b.push(b'\t'),
            Key::Backspace => b.push(0x7f),
            Key::Paste(s) => b.extend_from_slice(s.as_bytes()),
            Key::Ctrl(c) if c.is_ascii_lowercase() => b.push(c - b'a' + 1),
            _ => {}
        }
    }
    b
}
