# Changelog

All notable changes to aish are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

New entries go under **Unreleased**. At release time that heading becomes the
version and date, and the release notes on GitHub are taken from it.

## [Unreleased]

### Added
- Chat mode: a bare `?` makes every new line start with `? ` (hint `chat · Esc exit`);
  delete the `? ` to run one bash command; Enter or Esc on a bare `? ` leaves.
  A bare `?` no longer asks about the last output: just ask (`? why`).

### Changed
- A remote model without a known context window is assumed to have 32k tokens
  (was 8k), so the conversation is no longer compacted every few questions.
- On a narrow screen the model is told the width and keeps its lines short.
- Small local models gather facts with their tools far more reliably (a short
  nudge next to the question).

## [0.4.2] - 2026-11-28

### Added
- The model can `ask` you a question (with optional numbered choices; answer
  in the input line, which shows `answer · Esc skip` meanwhile) and `insert` a
  command into your input line for you to run or edit.

### Changed
- With tools on, the final ```bash block of an answer is no longer put into
  the input line; the model hands commands over with `insert`.
- In judge mode `ssh`, `scp`, `sftp`, `rsync`, `sshpass` and `mosh` always ask
  you, whatever the judge says.

### Fixed
- Resizing the terminal (a phone keyboard shown or hidden) while the model
  waits for your approval or answers no longer lets bash repaint its prompt over
  it; the hidden approval looked idle while it still waited for Enter.
- An empty answer from the model shows a dim `empty answer` note.

## [0.4.1] - 2026-11-28

### Changed
- The model answers with facts it gathers itself (read-only commands, files,
  the terminal history, the web) instead of listing commands to try; asked how
  to do something, it explains and offers to do it.

### Added
- The model always replies in English, like aish itself, whatever the
  question's language; `language:` in the config picks another one.

## [0.4.0] - 2026-11-28

### Removed
- The legacy `?!` question form (`?n` starts a new conversation; `?!` is now an
  ordinary shell line).
- The reserved `tools: {skills: ...}` switch; it did nothing. An old config that
  still has it keeps working.

### Changed
- Smaller binary: 5.59 MB -> 4.78 MB (glibc release build) after dropping the
  regex, `time`, crossterm and `serde_yaml` dependencies (YAML now via the
  maintained `serde_norway` fork).
- Commands the model runs without asking are stricter: a `#` comment, `\r` or a
  line continuation now needs your approval, and anything under `~/.docker` is
  treated as secret.
- Files aish creates (its rc files, a new `~/.bashrc`, the first config) are
  private (0600).
- `RUST_LOG` takes `target=level` lists (`aish=debug`); other syntax falls back
  to `info`.

### Fixed
- `aish init` wrote `config.yaml` readable by everyone; it is now private
  (0600) like every other write of the config. If your config holds API keys,
  run `chmod 600 ~/.config/aish/config.yaml`.
- A crash inside `ai` help or status no longer leaves the terminal in the wrong
  mode.

## [0.3.3] - 2026-11-28

### Fixed
- A ghost suggestion that is a different whole command (`sudo apt install htop`
  after `sudo apt install <x>`) is no longer glued to the end of the line.
- The model answers the question asked rather than the last error, and its
  final command has no `<placeholders>`.

## [0.3.2] - 2026-11-28

### Changed
- The model knows it runs inside aish and reads the aish docs (or points to
  `ai help`) for questions about aish itself.

## [0.3.1] - 2026-11-28

### Fixed
- The model no longer claims, or offers to undo, a change it never made; when
  nothing was changed it says so.

## [0.3.0] - 2026-11-28

### Fixed
- A word like `-é` in a command no longer crashes the read-only check; paths
  through `/proc/<pid>/root` or `/cwd` are refused.
- The hard list (which turns a judge ALLOW into a question) now also sees a command
  behind any wrapper (`setarch x86_64 rm -rf`, `env -S`, `script -c`), `git -c`,
  interpreters and shell-state builtins.
- Read-only commands that run unasked are stricter: git only as whole commands
  (`status`, `diff --stat`, `log --oneline -5` ...), no `cargo metadata` or
  `journalctl`, paths resolved as the kernel does (no `/dev/fd/../..` into another
  process's environment).
- Commands too long to be shown whole (12 rows, 1000 characters) are refused in every
  mode, before anything is judged or asked.
- If bash never starts the command aish typed (syntax error, cancelled quote), the
  model is told it did not run; the next command you run is no longer taken for its
  result, and a command that comes back changed is no longer taken for it either.
- Keys typed before an approval prompt appeared count towards the 700 ms quiet window,
  so finishing a line with Enter cannot approve a command.
- `AISH_JUDGE_TIMEOUT_MS` works in debug builds only.
- A refused `read_file` says why (`read_file refused: secret file · ~/.ssh/…`),
  reason first so it survives narrow screens.
- Tool lines (`read_file`, `run`, `⚖`, …) are cut to the screen width and
  never wrap.

### Added
- `run: judge`: a model decides instead of you. Whatever is not read-only goes to a
  one-shot judge (no tools, no history; sees the command, cwd and your last
  question) that answers ALLOW / ASK / DENY with a short reason: `⚖ allowed · …` runs
  it, `⚖ denied · …` refuses it (the agent is told why), ASK, garbage, an error or an
  8 s timeout shows the prompt with the reason. A command on the hard list (sudo/su, `rm -r/-f`, dd, mkfs, fdisk/parted, shutdown, `curl | sh`,
  `chmod/chown -R`, `git push --force`, `git -c`, `!` history expansion, interpreters, shell state,
  also behind any wrapper) never runs on ALLOW: you decide.
  `run:` also takes `{mode, judge, judge_prompt: {append|replace}}`; `ai run judge
  [--save]`; the wait shows `judging`; local llama.cpp gets a GBNF grammar. In `ask`,
  `s` switches the session to `judge` and `a` saves `run: judge`; an unknown `run:`
  value means `ask`.
- `prefix:` in the config picks the character that starts a question: `?` (default),
  `,`, `@`, `#` or `;`. Detection, highlighting, the right-edge marker and all hints
  follow it; anything else is a config error that names the allowed set.
- Conversation commands: `?n` new conversation (`?!` stays as a hidden alias for one
  release), `?c` compact now, `?h` short help; the forms are also in `ai help`.
- Auto-compaction: `compact:` (`model`, `at`, `size`, `prompt`). Past `at` of the chat
  model's context window the next `?` summarises the memory first (`◇ compacted
  12.4k → 1.1k tokens`); on failure the oldest turns are dropped. The window comes from
  `context_window`, a local llama.cpp's `/props`, or defaults to 8192, and `ai status`
  shows it (`ctx 32k`).

### Changed — UI text is English
- Every string aish prints (hints, notices, `ai help`/`models`/`profile`, errors, `init`
  and first-run lines, the waiting indicator, the overlay, the update notice) is
  English; the conversation with the model still follows your language
  (the system prompt says so). Docs are English too.

### Fixed
- `aish init` puts its `.bashrc` block at the very top and execs the binary by
  absolute path, so the outer bash runs nothing first: banners and other output
  from `.bashrc` no longer appear twice. Rerun `aish init` to move an old block.

### Added
- The model may run commands itself, with your permission. For a command that is not
  read-only the agent shows a dim prompt under the answer (`│ ▶ cmd`, `Enter run ·
  s session · a always · Esc no`). Enter types the command into your bash as if you
  did (visible, sudo can prompt, `cd` persists, Ctrl-C works) and the model gets the
  exit code, the output tail and the duration; `s` switches to `judge` until aish
  exits, `a` also saves `run: judge`, Esc declines (the model is told and goes on). It
  is typed only into an empty line at an idle prompt. `ai run` shows the mode, `ai run
  ask|judge|off` sets it (`--save` keeps it); `off` is the old behaviour (the command
  goes into your input line). `run:` in the config.
- Per-tool switches: `tools: {run, files, web, skills}` in the config (`skills` is
  reserved), `ai tools` lists them, `ai tools web off [--save]` changes one. An off
  tool is not sent to the model and not mentioned in its prompt; a call to it is
  refused. `tools: {run: off}` means no run tool at all (not even read-only commands),
  unlike `ai run off`.
- A waiting indicator for `?` answers (also in the Ctrl-Q overlay): one dim
  status row, `│ ⠹ waiting · 4s`, updated in place and replaced by the
  answer; phases connecting / waiting / thinking / loading model (llama.cpp
  router mode), `Ctrl-C — cancel` after 10 s when it fits; cut to the width.
- Bare model aliases: `ai chat ds` works when only one provider has `ds`
  (`provider/alias` still works and is required when names collide); roles in
  the config accept them too, and `ai chat` suggests them.
- `ai help` (and `ai -h`), `ai models`, `ai model [chat|complete] <model>` and
  `ai profile [name]`; `ai chat|complete <model>` now saves the choice to the
  config too (only the role's lines are edited, comments and the rest stay;
  if the file's shape is unusual the change is for the session only).
- Fallback models: a role may be a list (`chat: {id: [mellum, ds]}`); when a
  model refuses, times out, answers 5xx or 404 before any output, the next one
  answers (`↪ ds · mellum unavailable`) and the failed one is skipped for 60 s,
  also for `complete`, silently. Secret masking follows the model that answers.
- Profiles: `profiles:` and `profile:` in the config choose sets of roles.
- Lines starting with `ai` never reach bash or the model: anything but a
  subcommand prints a dim `did you mean chat?` (when close), `ask the model: ? <question>`
  and the help (a bare `ai` shows the help too). Unknown models, roles and profiles
  get a dim hint with the closest name; `ai`/`aish` are no longer highlighted as unknown
  commands; ghost hint `→ chat` for a near-miss.

### Fixed
- `aish status`: a space between `complete` and the model name.

## [0.2.1] - 2026-11-28

### Fixed
- Narrow terminals (phones, ~36 columns): a prompt that exactly fills the row
  no longer disables ghost text, the marker, Ctrl-R, Tab and notices (UTF-8).
- Notices, the first-run line and the Ctrl-Q overlay are cut to the width;
  multi-row and two-line prompts keep their rows, and a `?` answer is no
  longer overwritten by the repainted prompt; cleaner redraw after a resize.
- bash 5.3: typing after a terminal resize no longer overwrites the prompt.
- The first-run and update lines show up as soon as they are ready, also when
  the background check finishes after the prompt was drawn.

## [0.2.0] - 2026-11-27

### Added
- Install and update straight from GitHub: a signed apt repository on GitHub Pages
  (`https://aadegtyarev.github.io/aish`), `curl -fsSL .../install.sh | bash`
  (apt on Debian/Ubuntu, else a checksum-verified tarball into `~/.local/bin`), and
  `aish update [--check]` for tarball installs (a package install prints the apt command).
- A quiet once-a-day background check for a newer release: one dim line
  ` ↑ aish X.Y.Z — aish update`; `update_check: off` disables it.
- Web tools for the agent: `web_search` and `web_read`, each with a configurable
  chain of backends (provider's own search, DuckDuckGo, Brave; direct fetch with
  SSRF protection, Jina Reader). Read-only, run without asking, also in the
  Ctrl-Q overlay; queries and URLs are masked. Config: `web:` (`web: off` to
  disable), per model `web: native`.
- Ctrl-Q asks the model from anywhere, also in the middle of a running command
  (ssh, REPLs, docker exec): an overlay with the command's recent output that
  can type the answer's command into the program (never pressing Enter).
  Configurable with `keys: {ask: ...}`; at the prompt it toggles `? `.

## [0.1.0] - 2026-11-27

First public release.

### Added
- Bash in a PTY with shell integration: command blocks with output, exit code,
  cwd and duration.
- Own line editor: history ghost suggestions, inline fuzzy Ctrl-R, syntax
  highlighting, Tab completion from bash in a compact pager.
- `?` asks the model about the last output or the shell: `?`, `? text`, `?N`,
  `?!` to forget the conversation. A suggested command lands in the prompt and
  runs only on Enter.
- Agent tools: read-only commands run automatically from an allowlist, plus
  reading blocks, jobs and files. Secrets are masked for cloud providers.
- Background jobs tracking with short notices (` ✓ [1] cmd · 2m13s`).
- OpenAI-compatible providers, chat and responses APIs, local and cloud;
  separate models for completion and chat; simple YAML config.
- `ai` control command (name configurable or off), with argument suggestions.
- First run detects llama.cpp, ollama, LM Studio, `OPENAI_API_KEY` and
  `DEEPSEEK_API_KEY`.
- `aish init` hooks aish into `.bashrc` and `.tmux.conf`; `--remove` undoes it.
- Line editor as a separate MIT library crate with plugin hooks
  (`crates/aish-editor`, not on crates.io yet).
- Static musl binaries and deb packages for x86_64 and arm64.

[Unreleased]: https://github.com/aadegtyarev/aish/compare/v0.4.2...HEAD
[0.4.2]: https://github.com/aadegtyarev/aish/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/aadegtyarev/aish/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/aadegtyarev/aish/compare/v0.3.3...v0.4.0
[0.3.3]: https://github.com/aadegtyarev/aish/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/aadegtyarev/aish/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/aadegtyarev/aish/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/aadegtyarev/aish/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/aadegtyarev/aish/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/aadegtyarev/aish/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/aadegtyarev/aish/releases/tag/v0.1.0
