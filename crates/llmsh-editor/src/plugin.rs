//! The plugin interface. Everything that is not basic editing (history
//! suggestions, Ctrl-R, the completion pager, syntax highlighting, host
//! specific behavior) is a [`Plugin`].
//!
//! Hooks are optional (default no-ops) and are called on the thread that feeds
//! the editor. A hook must never block: slow work is done elsewhere and its
//! result delivered through a [`Handle`]; the editor drops results that were
//! produced for an older version of the line (generation counter).
//!
//! The hooks are plain data in/out (keys, text, spans, rows), so the same
//! shape can later be spoken to an out-of-process plugin.

use std::any::Any;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::buffer::Buffer;
use crate::highlight::Span;
use crate::keys::Key;
use crate::theme::Theme;

/// Whether a hook consumed its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// Consumed: no later plugin and not the core sees it.
    Yes,
    /// Not interested.
    Pass,
}

/// Verdict of [`Plugin::on_submit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitAction {
    /// Let the line through.
    Submit,
    /// The host handles this line itself (reported as `Event::Intercepted`).
    Intercept,
    /// Continue with this text instead.
    Edit(String),
}

/// Extra content a plugin contributes below (or instead of) the input line.
#[derive(Default)]
pub struct Decoration {
    pub(crate) rows: Vec<u8>,
    pub(crate) nrows: u16,
    pub(crate) input: Option<(String, usize)>,
    pub(crate) hide_ghost: bool,
}

impl Decoration {
    /// Add a row below the input. `f` writes the row's bytes (SGR allowed, no
    /// newline); it must fit the terminal width given to `decorate`.
    pub fn push_row(&mut self, f: impl FnOnce(&mut Vec<u8>)) {
        if self.nrows > 0 {
            self.rows.push(b'\n');
        }
        f(&mut self.rows);
        self.nrows += 1;
    }

    /// Show `text` (cursor at byte `cursor`) on the input line instead of the
    /// buffer, e.g. a search query.
    pub fn show_input(&mut self, text: &str, cursor: usize) {
        self.input = Some((text.to_owned(), cursor));
    }

    /// Do not draw suggestion text.
    pub fn hide_ghost(&mut self) {
        self.hide_ghost = true;
    }
}

pub(crate) struct Delivery {
    pub plugin: usize,
    pub generation: u64,
    pub payload: Box<dyn Any + Send>,
}

type Waker = Arc<dyn Fn() + Send + Sync>;

/// Lets a plugin hand results to the editor from any thread. Cloneable.
#[derive(Clone)]
pub struct Handle {
    pub(crate) queue: Arc<Mutex<VecDeque<Delivery>>>,
    pub(crate) plugin: usize,
    pub(crate) waker: Option<Waker>,
}

impl Handle {
    /// Queue `payload` for [`Plugin::on_delivery`], valid for line version
    /// `generation` (see [`Ctx::generation`]); the host is woken (see
    /// `EditorBuilder::waker`) and calls `Editor::poll`.
    pub fn deliver(&self, generation: u64, payload: Box<dyn Any + Send>) {
        if let Ok(mut q) = self.queue.lock() {
            q.push_back(Delivery {
                plugin: self.plugin,
                generation,
                payload,
            });
        }
        if let Some(w) = &self.waker {
            w();
        }
    }
}

/// What a hook can see and change.
pub struct Ctx<'a> {
    pub(crate) buf: &'a mut Buffer,
    pub(crate) theme: &'a Theme,
    pub(crate) handle: Handle,
    pub(crate) redraw: bool,
}

impl Ctx<'_> {
    /// The line.
    pub fn text(&self) -> &str {
        self.buf.text()
    }
    /// Cursor as a byte offset.
    pub fn cursor(&self) -> usize {
        self.buf.cursor()
    }
    /// Replace the whole line; cursor goes to the end. Undoable.
    pub fn set_text(&mut self, s: &str) {
        self.buf.set(s);
    }
    /// Move the cursor.
    pub fn set_cursor(&mut self, c: usize) {
        self.buf.set_cursor(c);
    }
    /// Insert at the cursor.
    pub fn insert(&mut self, s: &str) {
        self.buf.insert_str(s);
    }
    /// Replace `start..end` by `with` and put the cursor after it.
    pub fn replace_range(&mut self, start: usize, end: usize, with: &str) {
        let t = self.buf.text();
        let mut n = String::with_capacity(t.len() + with.len());
        n.push_str(&t[..start]);
        n.push_str(with);
        n.push_str(&t[end..]);
        self.buf.set(&n);
        self.buf.set_cursor(start + with.len());
    }
    /// Version of the line; changes on every edit. Pass it to
    /// [`Handle::deliver`] for work started now.
    pub fn generation(&self) -> u64 {
        self.buf.rev()
    }
    /// Colors and attributes in use.
    pub fn theme(&self) -> &Theme {
        self.theme
    }
    /// Handle for delivering results later.
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }
    /// Ask for a redraw even if nothing in the line changed.
    pub fn request_redraw(&mut self) {
        self.redraw = true;
    }
}

/// A feature of the editor. Implement what you need.
pub trait Plugin: Send {
    /// Unique short name.
    fn name(&self) -> &'static str;

    /// Higher runs first (key handling, suggestions) and is painted over by
    /// nobody (highlighting: higher wins). Default 0.
    fn priority(&self) -> i32 {
        0
    }

    /// Named actions this plugin implements with the keys bound to them by
    /// default. Users rebind through the `Keymap`.
    fn bindings(&self) -> &'static [(Key, &'static str)] {
        &[]
    }

    /// A key press, before the keymap. Return `Yes` to own it (modal UI).
    fn on_key(&mut self, _ctx: &mut Ctx, _key: &Key) -> Handled {
        Handled::Pass
    }

    /// A key bound to one of [`Plugin::bindings`] names (or by the user).
    fn on_action(&mut self, _ctx: &mut Ctx, _action: &str) -> Handled {
        Handled::Pass
    }

    /// The line changed (after the hooks of a key press ran).
    fn on_buffer_change(&mut self, _ctx: &mut Ctx) {}

    /// Ghost text: the whole suggested line, extending `ctx.text()`. Called
    /// on every redraw with the cursor at the end; keep it cheap. Async
    /// sources return their last delivered result.
    fn suggest(&mut self, _ctx: &mut Ctx) -> Option<&str> {
        None
    }

    /// Style spans for `text`. Overlaps resolve in favor of higher priority.
    fn highlight(&self, _text: &str, _out: &mut Vec<Span>) {}

    /// Rows below the input, or an alternative input line.
    fn decorate(&mut self, _ctx: &mut Ctx, _width: u16, _out: &mut Decoration) {}

    /// The user pressed Enter. All plugins are consulted in order.
    fn on_submit(&mut self, _ctx: &mut Ctx, _line: &str) -> SubmitAction {
        SubmitAction::Submit
    }

    /// A result sent through this plugin's [`Handle`] arrived and is still current.
    fn on_delivery(&mut self, _ctx: &mut Ctx, _payload: Box<dyn Any + Send>) {}
}
