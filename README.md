# llmsh

bash wrapper with LLM assistance. With no LLM use it behaves like plain bash.
Spec: [docs/UX.md](docs/UX.md), [docs/config.example.yaml](docs/config.example.yaml).

## Run

```
./dev.sh build            # cargo in docker (rust:1.93.1-bookworm); also: test, clippy -- -D warnings, fmt
./target/debug/llmsh      # runs on the host
```

Config: `$XDG_CONFIG_HOME/llmsh/config.yaml` (optional). Log: `~/.local/state/llmsh/llmsh.log`
(`RUST_LOG=llmsh=debug` logs every finished block).

## Design

- `llmsh` spawns `bash --rcfile <generated> -i` on a pty sized like the host terminal and
  puts the host terminal in raw mode (restored on exit and on panic).
- Two blocking threads on raw fds, no async runtime, no buffering:
  stdin -> pty master (4 KB stack buffer, no allocation), and pty master -> stdout
  (64 KB reusable buffer). SIGWINCH is forwarded to the pty. Ctrl-C/Z are plain bytes.
- Output path: read, strip llmsh-private OSC (`osc::Filter`), write to the terminal,
  and only then feed the same chunk to the `vte`-based `blocks::BlockTracker`,
  so parsing never delays display. Exit code is bash's.
- The generated rc sources `/etc/bash.bashrc` and `~/.bashrc`, then wraps `PROMPT_COMMAND`
  (string or array) with a first and a last hook, wraps `PS1` with OSC 133 A/B and sets `PS0`
  to emit C. The first hook sees the real `$?`, emits the command text
  (`OSC 1337;llmsh;p=<percent-encoded>`, stripped before reaching the terminal),
  `D;<exit>`, and the status line (` ✗ 101 · 14.2s`, ` ✓ 14.2s` only for exit != 0 or > 5 s).
  OSC 133 and OSC 7 are forwarded as-is.
- Command text comes from `history 1`; commands skipped by `HISTCONTROL`/`set +o history`
  are recorded with an empty command.

## Line editor (prompt mode)

bash prints its prompt; llmsh owns only the input area. After OSC 133;B and a short
output silence (10 ms) llmsh switches to prompt mode: keys go to the line editor
(`crates/llmsh-editor`, a standalone library crate with a plugin architecture),
bash's readline sees nothing until Enter, then the line is typed into bash
(`line\r`), so history, PS2 and everything else stay native. Anything typed before
the mode switch, `TERM=dumb`, `LLMSH_EDITOR=0`, or any program that is running
(vim, ssh...) is plain passthrough. Output arriving while editing (background job)
makes bash repaint its prompt below it and the line is redrawn.
`?`-lines (see UX.md) are not sent to bash yet: a dim notice is printed.
Robustness rules: our OSC 133 marks carry a per-session nonce (`llmsh=<id>`), so marks
of foreign shells (ssh, docker exec, nested integrations) never start prompt mode. If bash
echoes anything after the prompt mark (type-ahead typed during the previous command), that
prompt stays plain bash (no ghost/highlight) instead of guessing readline's buffer. Bytes
for bash are never written while the controller lock is held, and a panic in the editor or
a plugin drops the session to plain passthrough. The shell reports its PATH (private OSC)
whenever it changes, which feeds the highlighter. A submitted line is typed into bash as one
bracketed paste when bash has it on (multi-line stays one history entry).
Features are modules (history, highlight, completion, model), see
[docs/PLUGINS.md](docs/PLUGINS.md); config `modules: [..]` selects a subset.

| Keys | Action |
|---|---|
| ←/→, Ctrl-B/F, Home/End, Ctrl-A/E | move (graphemes); → / End / Ctrl-F at end accept the ghost suggestion |
| Alt-B/F, Ctrl-←/→, Alt-←/→ | word move; Alt-→ / Alt-F at end accept one word of the ghost |
| Backspace, Ctrl-H, Del, Ctrl-D (empty line: EOF to bash) | delete |
| Ctrl-W, Alt-Backspace, Alt-D, Ctrl-U, Ctrl-K, Ctrl-Y | kill / yank |
| Ctrl-_ , Ctrl-T | undo, transpose |
| ↑/↓, Ctrl-P/N | history (prefix-filtered if the line is not empty) |
| Ctrl-R | inline fuzzy history search (↑/↓/Ctrl-R select, Enter -> line, Esc/Ctrl-G cancel) |
| Tab | bash completion (bash-completion + your ~/.bashrc in a sidecar); single match inserts, common prefix inserts, else a pager: Tab/Shift-Tab/arrows cycle with preview, Enter/printable accept, Esc reverts |
| Ctrl-C / Ctrl-L | clear line + `^C` to bash / clear screen |
| Bracketed paste | inserted verbatim |

History comes from `$HISTFILE` (default `~/.bash_history`). Highlighting: unknown first word
red, strings yellow, `$vars` cyan, comments dim; `NO_COLOR` disables colors and the ghost text.
Completion candidates come from a long-lived `bash --norc -i` sidecar (pipes, no pty, own session)
that sources bash-completion and `~/.bashrc`; answers arrive asynchronously (300 ms limit, then
plain file names). Aliases/functions/PATH of the sidecar feed the highlighter.

## License

MIT.
