//! Key bindings: which [`Command`] or plugin action a [`Key`] runs.

use std::collections::HashMap;

use crate::keys::Key;

/// An editing action that can be bound to a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Command {
    /// Finish the line.
    Accept,
    /// One grapheme left.
    CharLeft,
    /// One grapheme right; at the end of the line accepts the suggestion.
    CharRight,
    /// One word left.
    WordLeft,
    /// One word right; at the end of the line accepts one suggested word.
    WordRight,
    /// Start of line.
    LineStart,
    /// End of line; at the end accepts the whole suggestion.
    LineEnd,
    /// Delete the grapheme before the cursor.
    DeleteBack,
    /// Delete the grapheme under the cursor.
    DeleteForward,
    /// Like `DeleteForward`, but on an empty line signals end of input.
    DeleteOrEof,
    /// Kill back to whitespace (unix-word-rubout).
    KillWordBackWs,
    /// Kill the previous word.
    KillWordBack,
    /// Kill the next word.
    KillWordForward,
    /// Kill to end of line.
    KillToEnd,
    /// Kill to start of line.
    KillToStart,
    /// Insert the last killed text.
    Yank,
    /// Undo the last edit.
    Undo,
    /// Swap the characters around the cursor.
    Transpose,
    /// Clear the line and signal an interrupt.
    Interrupt,
    /// Signal a clear-screen request.
    ClearScreen,
}

/// What a key does: a core [`Command`] or an action name implemented by a
/// plugin (e.g. `"history-search"`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Binding {
    /// Core editing command.
    Command(Command),
    /// Named plugin action.
    Action(String),
}

/// Maps keys to bindings. [`Keymap::emacs`] is the default; plugins add their
/// default keys for names nobody bound.
#[derive(Debug, Clone)]
pub struct Keymap {
    map: HashMap<Key, Binding>,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap::emacs()
    }
}

impl Keymap {
    /// An empty map (printable keys still insert themselves).
    pub fn empty() -> Self {
        Keymap {
            map: HashMap::new(),
        }
    }

    /// Emacs-style bindings.
    pub fn emacs() -> Self {
        use Command::*;
        let mut m = Keymap::empty();
        for (k, c) in [
            (Key::Enter, Accept),
            (Key::Left, CharLeft),
            (Key::Ctrl(b'b'), CharLeft),
            (Key::Right, CharRight),
            (Key::Ctrl(b'f'), CharRight),
            (Key::AltLeft, WordLeft),
            (Key::CtrlLeft, WordLeft),
            (Key::Alt('b'), WordLeft),
            (Key::AltRight, WordRight),
            (Key::CtrlRight, WordRight),
            (Key::Alt('f'), WordRight),
            (Key::Home, LineStart),
            (Key::Ctrl(b'a'), LineStart),
            (Key::End, LineEnd),
            (Key::Ctrl(b'e'), LineEnd),
            (Key::Backspace, DeleteBack),
            (Key::Delete, DeleteForward),
            (Key::Ctrl(b'd'), DeleteOrEof),
            (Key::Ctrl(b'w'), KillWordBackWs),
            (Key::AltBackspace, KillWordBack),
            (Key::Alt('d'), KillWordForward),
            (Key::Ctrl(b'k'), KillToEnd),
            (Key::Ctrl(b'u'), KillToStart),
            (Key::Ctrl(b'y'), Yank),
            (Key::Ctrl(b'_'), Undo),
            (Key::Ctrl(b't'), Transpose),
            (Key::Ctrl(b'c'), Interrupt),
            (Key::Ctrl(b'l'), ClearScreen),
        ] {
            m.bind(k, c);
        }
        m
    }

    /// Bind (or rebind) `key` to a core command.
    pub fn bind(&mut self, key: Key, cmd: Command) {
        self.map.insert(key, Binding::Command(cmd));
    }

    /// Bind `key` to a plugin action by name.
    pub fn bind_action(&mut self, key: Key, action: &str) {
        self.map.insert(key, Binding::Action(action.to_owned()));
    }

    /// The binding of `key`.
    pub fn get(&self, key: &Key) -> Option<&Binding> {
        self.map.get(key)
    }
}
