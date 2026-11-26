# lineedit-wip (placeholder name)

A small, embeddable emacs-style line editor for Rust, built as a core plus
plugins. Basic editing (graphemes, kill ring, undo, bracketed paste) is in the
core; history suggestions, fuzzy Ctrl-R, a completion pager and syntax
highlighting are ordinary plugins you register. Dependencies: `unicode-segmentation`,
`unicode-width`, and `libc` (raw mode / window size, standalone reader only).
No async runtime: slow sources answer later through a `Handle` and stale
answers are dropped.

Two modes: **standalone** (`LineReader` prints a prompt and reads the tty, like
rustyline/reedline) and **embedded** (the host owns the terminal and the
prompt; feed bytes to `Editor::input`, draw with `Editor::draw`, react to `Event`s).

```rust
use lineedit_wip::plugins::{HistorySearch, HistorySuggest, SharedHistory, SyntaxHighlight};
use lineedit_wip::{Editor, LineReader, MemHistory, Prompt, Signal};

fn main() -> std::io::Result<()> {
    let hist = SharedHistory::new(Box::new(MemHistory::parse("ls -la\ngit status\n")));
    let editor = Editor::builder()
        .with(HistorySuggest::new(hist.clone())) // ghost text, Up/Down
        .with(HistorySearch::new(hist)) // Ctrl-R
        .with(SyntaxHighlight::new(|w| w == "ls" || w == "git"))
        .build();
    let mut reader = LineReader::new(editor);
    loop {
        match reader.read_line(&Prompt::new("> "))? {
            Signal::Line(l) => println!("got {l}"),
            Signal::Eof => break,
            _ => {}
        }
    }
    Ok(())
}
```

See `examples/standalone.rs` for history file, Tab completion and a right prompt.

## Customizing

* `Keymap` (emacs by default): `bind(Key, Command)` for core commands,
  `bind_action(Key, "history-search")` for actions plugins register.
* `Theme`: SGR strings per element, 16-color defaults, `Theme::from_env()` honors `NO_COLOR`.
* `Plugin` trait: optional hooks `on_key`, `on_action`, `suggest`, `highlight`,
  `decorate`, `on_submit`, `on_buffer_change`, `on_delivery`; `Intercept` plugin
  lets the host claim lines instead of submitting them.

License: MIT OR Apache-2.0.
