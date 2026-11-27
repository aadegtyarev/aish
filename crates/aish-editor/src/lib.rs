//! A small, embeddable emacs-style line editor.
//!
//! The core handles basic editing (cursor motion over grapheme clusters, kill
//! ring, undo, bracketed paste); everything else is a [`Plugin`]: history
//! suggestions, fuzzy history search, a completion pager, syntax highlighting.
//! Two ways to use it:
//!
//! * **Embedded**: the host already printed a prompt and owns the terminal.
//!   Feed keyboard bytes to [`Editor::input`], draw with [`Editor::draw`] and
//!   react to [`Event`]s. The editor only touches the input area, starting at
//!   the column the host gives it.
//! * **Standalone**: [`LineReader`] prints a prompt, switches the terminal to
//!   raw mode and returns finished lines.
//!
//! Plugins never block key handling: slow work is delivered later through a
//! [`Handle`] and stale results are dropped.

#![warn(missing_docs)]

mod buffer;
pub mod completion;
mod editor;
mod highlight;
mod history;
mod keymap;
mod keys;
mod plugin;
pub mod plugins;
mod render;
pub mod standalone;
mod theme;

pub use editor::{Editor, EditorBuilder, Event};
pub use highlight::{Class, Span};
pub use history::{fuzzy, History, MemHistory};
pub use keymap::{Binding, Command, Keymap};
pub use keys::{CursorReportFilter, Key};
pub use plugin::{Ctx, Decoration, Handle, Handled, Plugin, Row, RowStyle, SubmitAction};
pub use standalone::{LineReader, Prompt, Signal};
pub use theme::Theme;

#[cfg(test)]
mod tests;
