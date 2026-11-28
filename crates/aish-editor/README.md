# aish-editor

A small, embeddable emacs-style line editor for Rust, built as a core plus
plugins. Basic editing (graphemes, kill ring, undo, bracketed paste) is in the
core; history suggestions, fuzzy Ctrl-R, a completion pager and syntax
highlighting are ordinary plugins you register. Dependencies: `unicode-segmentation`,
`unicode-width`.
No async runtime: slow sources answer later through a `Handle` and stale
answers are dropped.

The host owns the terminal and the prompt: feed bytes to `Editor::input`, draw with `Editor::draw`, react to `Event`s.

```rust
use aish_editor::plugins::{HistorySearch, HistorySuggest, SharedHistory, SyntaxHighlight};
use aish_editor::{Editor, MemHistory};

fn main() -> std::io::Result<()> {
    let hist = SharedHistory::new(Box::new(MemHistory::parse("ls -la\ngit status\n")));
    let editor = Editor::builder()
        .with(HistorySuggest::new(hist.clone())) // ghost text, Up/Down
        .with(HistorySearch::new(hist)) // Ctrl-R
        .with(SyntaxHighlight::new(|w| w == "ls" || w == "git"))
        .build();
    // feed bytes with editor.input(..), draw with editor.draw(..)
    let _ = editor;
    Ok(())
}
```

## Customizing

* `Keymap` (emacs by default): `bind(Key, Command)` for core commands,
  `bind_action(Key, "history-search")` for actions plugins register.
* `Theme`: SGR strings per element, 16-color defaults, `Theme::from_env()` honors `NO_COLOR`.
* `Plugin` trait: optional hooks `on_key`, `on_action`, `suggest`, `highlight`,
  `decorate`, `on_submit`, `on_buffer_change`, `on_delivery`; `Intercept` plugin
  lets the host claim lines instead of submitting them.

License: MIT.
