# aish

bash wrapper with LLM assistance. With no LLM use it behaves like plain bash.
Spec: [docs/UX.md](docs/UX.md), [docs/config.example.yaml](docs/config.example.yaml).

## Install

One line (Debian/Ubuntu: adds the signed apt repository and installs the package, asks once before
using sudo; elsewhere: verified release tarball into `~/.local/bin`; `--yes` skips the question):

```
curl -fsSL https://aadegtyarev.github.io/aish/install.sh | bash
```

Update: `sudo apt update && sudo apt install --only-upgrade aish` for the package, `aish update`
(`--check` only looks) for a tarball/`~/.local/bin` install. aish also shows one dim line
` ↑ aish X.Y.Z — aish update` at most once a day when a newer release exists (checked in the
background; `update_check: off` in the config disables it).

Debian/Ubuntu by hand (amd64, arm64) - the package registers `/usr/bin/aish` in `/etc/shells` and never touches
user files; run `aish init` yourself afterwards:

```
sudo dpkg -i aish_<ver>_amd64.deb      # or: sudo apt install ./aish_<ver>_amd64.deb
aish init
```

Static tarball (from the GitHub release; x86_64, aarch64; musl, no dependencies besides bash): unpack, then `./install.sh`
(installs the bundled binary to `~/.local/bin`, `--prefix DIR` to change, `--no-init` to skip setup).

From source:

```
./install.sh              # builds (cargo, or ./dev.sh in docker), copies to ~/.local/bin/aish, runs `aish init`
./install.sh --no-init    # skip the setup;  --bin PATH takes a prebuilt binary
```

`aish init` (also `ai init` in a session) asks up to three questions: where to start aish (every
interactive bash via a guarded block in `~/.bashrc`; also as tmux default shell; or only manually),
which models to use (autodetect: llama.cpp, ollama, LM Studio, `$OPENAI_API_KEY`, `$DEEPSEEK_API_KEY`;
keys are written as `$VAR` references, an existing config is kept unless you regenerate it), and
whether to apply the summary. Blocks are marked `# >>> aish >>>` / `# <<< aish <<<`, re-running
replaces them, the first change to a file leaves `<file>.aish-bak`. `aish init --remove` removes the
blocks; `--yes` applies the defaults without questions. Escape hatch: `AISH_DISABLE=1 bash` starts plain bash.
No `chsh`: for a login shell add aish's path to `/etc/shells` and run `chsh -s` yourself.

## Checks

`./check.sh` runs everything CI runs, in docker: fmt, clippy, tests on bash 5.2 and 5.3, cargo-deny,
cargo-machete, typos, shellcheck, actionlint; it stops at the first failure. `./check.sh --quick` is
fmt, clippy and the bash 5.2 tests. `./check.sh bench` builds release and measures startup and key-echo
latency against plain bash (also a weekly CI job).

## Release packaging

`./pack.sh [--arch x86_64|aarch64|all]` builds everything in docker (musl targets, zig + cargo-zigbuild
for cross-linking, cargo-deb) into `dist/`: tarballs, `.deb`s and `SHA256SUMS`. Pushing a `v*` tag runs
the same script in GitHub Actions and publishes a release. The version lives in the workspace `Cargo.toml`.

## Run

```
./dev.sh build            # cargo in docker (rust:1.93.1-bookworm); also: test, clippy -- -D warnings, fmt
./target/debug/aish      # runs on the host
```

Config: `$XDG_CONFIG_HOME/aish/config.yaml` (optional). Log: `~/.local/state/aish/aish.log`
(`RUST_LOG=aish=debug` logs every finished block).

## Design

- `aish` spawns `bash --rcfile <generated> -i` on a pty sized like the host terminal and
  puts the host terminal in raw mode (restored on exit and on panic).
- Two blocking threads on raw fds, no async runtime, no buffering:
  stdin -> pty master (4 KB stack buffer, no allocation), and pty master -> stdout
  (64 KB reusable buffer). SIGWINCH is forwarded to the pty. Ctrl-C/Z are plain bytes.
- Output path: read, strip aish-private OSC (`osc::Filter`), write to the terminal,
  and only then feed the same chunk to the `vte`-based `blocks::BlockTracker`,
  so parsing never delays display. Exit code is bash's.
- The generated rc sources `/etc/bash.bashrc` and `~/.bashrc`, then wraps `PROMPT_COMMAND`
  (string or array) with a first and a last hook, wraps `PS1` with OSC 133 A/B and sets `PS0`
  to emit C. The first hook sees the real `$?`, emits the command text
  (`OSC 1337;aish;p=<percent-encoded>`, stripped before reaching the terminal),
  `D;<exit>`, and the status line (` ✗ 101 · 14.2s`, ` ✓ 14.2s` only for exit != 0 or > 5 s).
  OSC 133 and OSC 7 are forwarded as-is.
- Command text comes from `history 1`; commands skipped by `HISTCONTROL`/`set +o history`
  are recorded with an empty command.

## Line editor (prompt mode)

bash prints its prompt; aish owns only the input area. After OSC 133;B and a short
output silence (10 ms) aish switches to prompt mode: keys go to the line editor
(`crates/aish-editor`, a standalone library crate with a plugin architecture),
bash's readline sees nothing until Enter, then the line is typed into bash
(`line\r`), so history, PS2 and everything else stay native. Anything typed before
the mode switch, `TERM=dumb`, `AISH_EDITOR=0`, or any program that is running
(vim, ssh...) is plain passthrough. Output arriving while editing (background job)
makes bash repaint its prompt below it and the line is redrawn.
Robustness rules: our OSC 133 marks carry a per-session nonce (`aish=<id>`), so marks
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

## Model features

- `?` alone turns chat mode on or off: every new line starts with `? ` (delete it to run one bash
  command; Enter or Esc on a bare `? ` leaves). `? text`, `?N text` ask the chat model (the character is `prefix:` in the config) (context: last N blocks trimmed to a token
  budget, cwd, OS, git). The answer streams under the line with a dim `│ `; Ctrl-C stops it
  (`│ ⏹`). A final ```bash block is not printed but put into the input line (Enter runs it,
  Ctrl-C clears). `?n` starts a fresh conversation, `?c` compacts it into a summary (also automatic near the context window, see `compact:`), `?h` is the short help. Secrets are masked before text goes to a
  non-local provider. Keys typed while it streams are applied afterwards.
- Language coach (`coach: fix|on`, off by default; `ai coach on|fix|off [--save]`): when your question
  is in the reply language (`language:`) and has mistakes, the answer starts with a dim
  `✎ Better: <corrected question>` line; `on` also completes the sentence as ghost text while you type
  a question. See docs/UX.md.
- Background jobs: when one finishes while you are at the prompt, a single line appears above it
  (` ✓ [1] cargo build · 2m13s` / ` ✗ [2] npm test · 41s · exit 1`) in place of bash's `[1]+ Done`; the hint at
  the right edge shows `[N] ?` while jobs run and `⋯` while a model suggestion is slow.
- Agent steps: when the chat model supports function calling, `?` can run read-only commands itself
  (a built-in allowlist plus `allow:` in the config; shown as a dim `│ $ cmd · N lines`), read files and
  command history, and propose one mutating command at a time into your input line: after you run it the
  model continues with its output. Max 12 steps, Ctrl-C stops. Models without tools fall back to the
  single-step answer (`tools: off` per model forces it). See docs/UX.md.
  The model can also `insert` a command into your input line (after you press Enter on it; nothing is
  run) and `ask` you a question: answer in the input line, a number picks a listed choice, Esc skips.
  With tools on, a final ```bash block is no longer prefilled (without tools it still is).
- `ai` (inside a session; always wins over any program of that name — rename or disable it with
  `command: mm` / `command: off` in the config) and its always-available long form `aish` show the roles and whether they answer;
  `ai help` lists the commands, `ai models` the models by provider, `ai model [chat|complete] <alias|provider/alias>`
  (also `ai chat <..>`, `ai complete <..>`) sets a role's model and saves it to the config (a role may list
  fallbacks: `chat: {id: [mellum, ds]}`), `ai profile [name]` lists/switches `profiles:`, `ai on|off` toggles model
  suggestions, `ai config` puts `${EDITOR:-vi} ~/.config/aish/config.yaml` in the line. `aish status` and
  `aish config` also work as CLI commands; any other arguments go to bash. Tab completes them.
- First start without a config probes ollama (`:11434`), LM Studio (`:1234`), llama.cpp `llama-server` (`:8080`) and `$OPENAI_API_KEY`
  (override URLs for tests with `AISH_PROBE_OLLAMA`, `AISH_PROBE_LMSTUDIO`, `AISH_PROBE_LLAMACPP`, `AISH_OPENAI_URL`),
  writes a config and prints one line. The config is re-read when its mtime changes.
- Model suggestions (role `complete`): a ghost plugin below history; 150 ms after the last key (Ctrl-Space: now)
  a worker asks for the continuation (cwd, last 5 commands, `ls` names; `max_tokens` 48, stop `\n`). Stale answers are
  dropped, results cached per line prefix, 3 failures in a row pause requests for 60 s. `→` / Alt-→ accept.
- Network I/O (crate `crates/aish-llm`, blocking `ureq` on worker threads) never runs on the key path.

- A dim `?` sits at the right edge of the input row while editing (hidden when the input gets within 3
  columns, when it wraps, and with `NO_COLOR`); it is erased on submit. Library: `plugins::RightHint` +
  `HintText` (host-settable text).

## License

MIT.
