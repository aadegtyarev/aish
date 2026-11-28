# aish — UX

> The best interface is no interface.

aish looks and behaves like your ordinary bash. No banners, panels, frames or
modes. The model appears only when it is needed, and disappears right after.

UI text is English only; the conversation follows the user's language.

## Principles

1. **Invisible by default.** If you don't talk to the model, aish is
   indistinguishable from `bash`. PS1, aliases, `.bashrc`, `vim`, `ssh` are all yours.
2. **2–3 steps to any action.** No settings menu, no dialogs.
3. **Propose, don't do.** The model puts a command into your input line, or, with
   your permission (`ai run`), types it into your bash. Nothing runs unasked except
   read-only commands; in `judge` mode the hard list (sudo, `rm -rf` ...) still asks.
4. **Instant.** Typing never waits for the network. LLM suggestions catch up,
   they don't block. Keypress latency < 1 ms, history < 5 ms.
5. **Quiet.** Network and model errors are one dim line, not a stack trace.
6. **The terminal's palette.** Only the 16 ANSI colors (the user's theme is
   respected), `dim` for secondary things, one accent. `NO_COLOR` turns it all off.

## What was borrowed, and from whom

| Decision | From | How it is adapted |
|---|---|---|
| Grey suggestion, `→` accepts | fish, zsh-autosuggestions | history instantly, the LLM catches up |
| `?` + natural language | Warp (AI command search) | one entry point: a question or a command |
| Command blocks (cmd/output/exit) | Warp, iTerm2 shell integration (OSC 133) | context for the model, "ask about the output above" |
| A command from the model goes into the input line | Butterfish, Copilot CLI | Enter = confirmation, no dialogs |
| Inline history search, not fullscreen | atuin (`--inline-height`), fzf `--height` | a list of 6–8 rows under the prompt |
| Duration and exit code | starship `cmd_duration`, `status` | only if > 5 s or exit ≠ 0 |
| Notice when a background job ends | zsh `notify`, undistract-me | one line above the prompt |
| Completion pager | fish | columns with flag descriptions |
| Zero configuration at the start | ollama/LM Studio autodetect | the first run finds the models itself |

## The one new gesture: `?`

A line `?` or `? text` (a question, then a space) goes to the model together
with context (the last blocks, cwd, `git status`).

In bash `?` is a glob, "any one character", so the rule is strict: only the
prefix alone, prefix + space + text, prefix + digits (`?3`, `?3 text`) and
prefix + one command letter (`?n`, `?c`, `?h`) go to the model. `?.txt`,
`?? *`, `?foo` and other glob patterns stay commands. To run a bare `?` as a
glob, escape it: `\?`.

### The prefix

`?` is the default; `prefix:` in config.yaml picks another one of `?` `,` `@`
`#` `;`. Everywhere below `?` means "your prefix": the recognition, the
highlighting, the dim marker at the right edge, the hints and `ai help`.
Chosen because none of them means anything at the start of a bash line
(`?` only as a one-character glob, `,foo`, `@foo` and `;` are just errors, `#`
is a comment). `#` is the one with a catch: `# text` (with a space) becomes a
question, so real comments need no space (`#text`) once you pick it.

Excluded, with the reason:

| Char | Why not |
|---|---|
| `.` | `. file` is `source` |
| `/` `~` | start of a path |
| `!` | history expansion |
| `%` | job spec (`%1`) |
| `^` | quick substitution (`^a^b`) |
| `:` | the shell no-op builtin |
| `-` | looks like an option, `-` alone is `cd -` in some setups |
| `*` | glob, matches files |
| `\` | escape, line continuation |
| `$` | expansion, a variable as a command |
| `&` | control operator, background |

A bad value is a config error that names the allowed set. In YAML quote it:
`prefix: ","`, `prefix: "#"`.

### Conversation commands

Letters right after the prefix, no space (`? n` with a space is a question):

| Line | Does |
|---|---|
| `?n` | new conversation: the memory is cleared, `◇ new conversation` |
| `?c` | compact now: the conversation is summarised and replaced by the summary, `◇ compacted 12.4k → 1.1k tokens` |
| `?h` | the forms above in six short rows (also at the end of `ai help`) |

### Compaction

The memory is the earlier `?` turns. When it grows past `compact.at` (default
70%) of the chat model's context window, the next `?` first summarises it:
`◇ ⠹ compacting…` (the waiting indicator, in place), then `◇ compacted a → b
tokens`, then the answer. The summary is one message at the head of the memory
and is folded into the next summary. The sizes are estimates (characters / 4); the check counts the memory plus the context, system prompt and tool schemas. `at` is at most 90%, the summary at most 2048 tokens. If any model of the chat role is non-local, the text sent to the summariser is masked.

```yaml
compact:
  model: local/qwen     # optional; default: the chat model in use
  at: 70%               # compact when the conversation exceeds this share of the window
  size: 10%             # target summary size as a share of the window
  prompt: |             # optional override of the default prompt
    ...
```

The window is the model's `context_window` from the config (at least 2048); unset and the
provider is a local llama.cpp, `n_ctx` from `GET <base>/props` (asked once per
model, ignored if absent); else 8192. `ai status` shows it (`ctx 32k`). The
summary request is masked like every other request: a non-local provider gets
masked text. If compaction fails, the oldest turns are dropped instead, with one
dim line (`◇ not compacted · dropped old turns`).

```
$ cargo build
error[E0425]: cannot find value `x` in this scope
 ✗ 101

$ ?                        ← an empty ?: "what's wrong here?" about the last block
 │ `x` is not declared in main.rs:12 — probably a typo, you meant `xs`.
 │ You can fix it like this:
$ sed -i '12s/\bx\b/xs/' src/main.rs        ← the command is already in the input line

$ ? find the 10 biggest files here
$ du -ah . | sort -rh | head -10             ← Enter runs it, editing works as usual
```

- The answer streams by lines with a dim bar `│` on the left. No frames.
- While the answer is on its way, one dim status row shows where it will start:
  `│ ⠹ waiting · 4s`. It is updated in place and replaced by the answer. Phases:
  `connecting`, `waiting` (request sent), `thinking` (only reasoning or
  keep-alive data so far), `loading model` (a local llama.cpp server in router
  mode reports the model as `unloaded`/`loading`). The elapsed time shows after
  1 s, `Ctrl-C — cancel` after 10 s; both are dropped first when the row does not
  fit. Without color the spinner is `.`/`..`/`...`. The Ctrl-Q overlay shows the
  same row; an error replaces it with the usual dim error line.
- If the answer contains a command, it is **inserted into the input line**.
  Enter runs it, Esc/Ctrl-C clears it, you can edit it.
- `?3 ...` takes the last 3 blocks into the context (default: 1).
- `Ctrl-C` during an answer cuts the stream.
- Continuing the dialogue: the next `?` in the session remembers the previous ones.
  `?n` starts from a clean slate.

### The agent step

If the model can call functions, `?` is a small agent loop. The model collects
facts itself and proposes commands that change things **one at a time**:

```
$ ? why doesn't it build
 │ Checking the status.
 │ $ git status · 12 lines                 ← a read-only command ran by itself
 │ Needs a build with logs:
$ cargo build 2>&1 | tail -30               ← in the input line; Enter runs it
...                                          ← the loop continues when the block ends
 │ The error is in main.rs:12, fixing…
```

- **Read-only commands** (an allowlist: `ls`, `cat`, `head`, `tail` without `-f`,
  `wc`, `grep`/`rg`, `find` without `-exec/-delete`, `stat`, `file`, `du`, `df`,
  `free`, `uname`, `whoami`, `id`, `ps`, `which`, `type`, `pwd`, `date`,
  `systemctl status`, and git only as whole commands: `status`, `status -s|--short|-sb`,
  `diff [--stat|--cached|--staged]`, `log --oneline [-N]`, `log -N`, `branch [-a]`,
  `remote -v`, `rev-parse --show-toplevel`; paths are resolved as the kernel does and
  secrets, other processes' files under `/proc` and wide recursions are refused) are
  run by aish itself, not in your terminal:
  `bash -c` in the current cwd, stdin `/dev/null`, 10 s, output up to 16 KB.
  The answer gets one dim line `│ $ cmd · N lines` (or `· exit 1`); the output
  goes to the model. Parsing uses a real tokenizer: `;`, `&`, `$(…)`, backticks,
  `<(…)`, redirects into files, environment assignments and a pipe into a
  command that is not allowed make a command "not read-only".
  Your own prefixes go in `allow:` in config.yaml. A planted `.git/config` (a clean
  filter, textconv, gpg.program) could still make these git commands run a program:
  the same risk as running the command yourself in that repository.
- **Everything else** goes through `run` and depends on the mode (`ai run`):
  - `ask` (default): a dim prompt under the answer, one key decides:
    ```
    │ ▶ apt list --upgradable
    │   Enter run · s session · a always · Esc no
    ```
    The command is shown as it is (control and bidi characters are refused, long
    commands wrap with the bar, at most 12 rows; the hint gets shorter on a narrow
    screen). **Enter** types the command into your bash as if you typed it: it is
    visible, its output is an ordinary block, sudo can ask for a password, `cd`
    persists, Ctrl-C stops it. The agent waits for the end of the block and gets
    the exit code, the output tail (masked for a non-local provider) and the
    duration (`interrupted by the user` after Ctrl-C). It is typed only when bash is
    at an idle prompt with an empty line: if you typed something meanwhile, nothing
    runs and the model is told so. If bash never starts the line (a syntax error, an
    unfinished quote you cancel), the model is told it did not run; what you run
    next is never taken for its result. **s** — the model runs commands without asking
    until aish exits and switches to `judge` (see below). **a** — the same, and `run: judge` is saved to the config
    (a targeted edit). **Esc** — declined: the model is told, it continues.
  - The prompt shows the whole command as it will be typed (newlines as `⏎`, edge or
    repeated blanks as `·`/`·×N`). In every mode, before anything is shown or
    judged, a command of more than 12 rows or 1000 characters is refused ("command
    too long to review": the model splits it). A key decides only as a single keypress
    (Enter, `s`, `a`, Esc), not in the first 300 ms and not within 700 ms of other
    typing (also typing before the prompt appeared); anything else shows `press
    Enter, s, a or Esc`.
  - `judge`: a one-shot **judge** model (`run.judge`, default the chat model; no tools,
    no history, 8 s; it sees only the command, the cwd and your last question, masked
    for a non-local provider) decides instead of you. It answers `ALLOW`/`ASK`/`DENY` +
    a reason: `│ ⚖ allowed · reason` runs the command, `│ ⚖ denied · reason` refuses it
    (the model is told why), `ASK` or no clean answer shows the prompt with `⚖ reason`
    above the keys. A command on the **hard list** never runs on ALLOW: it asks (sudo/su,
    `rm -r/-f`, dd, mkfs, fdisk/parted, shutdown/reboot, `curl | sh`, `chmod/chown -R`,
    `git push --force`, `git -c`, `reset --hard`, `clean -f`, interpreters, shell-state
    builtins, and any of these behind a wrapper such as `setarch`, `env -S`, `script
    -c`: every word of a command is looked at). The prompt has only Enter and Esc. The
    prompt can be extended or replaced with `run.judge_prompt`. While it thinks: `⠹
    judging`.
  - `off`: the old way — the command is put into the input line (the same checks
    as for a ```` ```bash ```` block) and the turn pauses. Run it (you may edit it;
    the model learns what was actually run), and when the block ends the model
    continues in a new `│` section. Ctrl-C on the line, or a different command
    instead of the proposed one, stops the agent silently.
  - `ai run` shows the mode, `ai run ask|judge|off` sets it for the session, add
    `--save` to write `run:` to the config. Read-only commands run by themselves
    in every mode. The Ctrl-Q overlay never has `run` (the foreground may be another host).
- Tools (`ai tools` switches `run`, `files`, `web`; an off tool is not sent to the
  model, not mentioned in its prompt, and a call to it is refused. `tools: {run: off}`
  means no run tool at all, even read-only; that differs from `ai run off`, which
  only stops the model from running other commands itself):
  `run(command, background?)`, `read_block(n)`, `jobs()`,
  `job_output(id, lines)`, `read_file(path, start_line?, max_lines?)`
  (up to 200 lines / 32 KB; secret files — `~/.ssh/*`, `*.pem`, `.env*`,
  `~/.aws/credentials`, `~/.netrc`, aish's own config — are refused).
  Background: `run(…, background=true)` puts `<cmd> &>…/jobs/<n>.log &` into the
  line (honest and visible), and `job_output` reads that log.
- Up to 12 steps per `?`, then a dim note. Ctrl-C stops the agent at any moment
  (`│ ⏹`). Command outputs are masked for a non-local provider, like the whole
  context.
- A model without function support: on HTTP 400 about tools/functions aish
  repeats the request once without tools and remembers that for the session
  (afterwards the old single-step mode with a ```` ```bash ```` block). Explicitly:
  `tools: off` on the model in the config.

## Ask from any program: `Ctrl-Q`

`?` works only at the prompt. When a command is already running — `ssh`, a REPL
(`python`, `psql`), `docker exec`, a long build — press **Ctrl-Q**. The key is
configurable: `keys: {ask: ctrl-q}` in config.yaml (`ctrl-<letter>` or `off`).

```
  ssh prod · last 200 lines             ← dim context: the command and how much output
? why is this slow                      ← an ordinary input line (emacs keys)
│ Looks like the disk is full of logs: ...
│   du -sh /var/log/* | sort -h | tail
  Enter — insert · Esc — close
```

- The overlay opens on the alternate screen: when closed, the program's screen
  is back as it was. If the program itself is on the alternate screen (vim,
  htop), the overlay is drawn on top, and on close the program is told to
  "redraw" (the window size changes for a moment and comes back; over ssh this
  arrives as a window-change).
- While the overlay is open the program's output is not read: it just blocks on
  write, nothing is lost. What piled up appears after closing. What you type in
  the meantime is not passed to the program.
- An empty Enter = a bare `?`: explain what is going on in the output. The
  context is the tail of the command's output (up to 200 lines), the command
  itself and cwd. For a program on the alternate screen the model is warned
  that the output stream may not match the picture.
- The tools are only `read_block`, `jobs`, `job_output`. No commands (`run`) and
  no file reading (`read_file`): the running command may be on another machine
  (ssh, docker exec), so local files and commands would be the wrong ones. The
  dialogue memory is shared with the ordinary `?`; secret masking for non-local
  providers is as usual.
- If the answer ends with a ```` ```bash ```` block, the command is shown and
  under it `Enter — insert`. Enter closes the overlay and **types the command
  into the program without Enter**; you are the one who runs it. A multi-line
  command is inserted only if the program enabled bracketed paste (otherwise it
  is shown, but without "insert").
- `Esc` closes. `Ctrl-C` cuts an answer in progress (a second one closes).
  Any typing after the answer is a follow-up question. `Ctrl-Q` on an empty line
  closes the overlay and sends the program one real Ctrl-Q (a double Ctrl-Q for
  emacs/micro). This is also the way to send the program a real XON/Ctrl-Q.

## Web: search and reading pages

The agent has two tools: `web_search(query)` and `web_read(url)`. Both only read
and run by themselves, without asking; each leaves one dim line in the answer:
`↗ search: query · duckduckgo`, `↗ host/path… · direct`. Only the backend that
worked is shown; if none worked — `↗ failed: reason`. They work in the `Ctrl-Q`
overlay too (they don't depend on the host) and count toward the turn's step limit.

Each tool goes through a chain of backends (`web:` in the config): search
`[provider, duckduckgo, brave]` (default `[provider, duckduckgo]`), reading
`[provider, direct, jina]` (default `[provider, direct]`). An unavailable backend
(no key, an empty result, a page with no text because of JS) is skipped silently.
`provider` is the model's built-in search: for OpenAI Responses it is on by
itself, for the others it is `web: native` on the model (the request goes out as
usual, and the model is assumed to search by itself); then our `web_search` is not
offered to the model, and `web_read` stays. `direct` goes only to public addresses
(loopback, LAN, link-local, CGNAT and the like are rejected, also after
redirects), without cookies, up to 2 MB. `web: off` turns both tools off.

Privacy: the query or URL goes to a third-party service (a search engine, Jina),
so it always passes through secret masking, even for a local provider; a URL that
masking changed is not opened. Page text is given to the model as untrusted data,
not as instructions.

## Suggestions while typing

```
$ git che█ckout -b feature/      ← grey: from history (instant)
$ docker run -it --rm █ubuntu bash   ← grey: from the model (after a ~150 ms pause)
```

| Key | Action |
|---|---|
| `→` / `End` / `Ctrl-F` | accept the whole suggestion |
| `Alt-→` / `Alt-F` | accept one word |
| `Tab` | bash completion (files, flags, branches), a pager like in fish |
| `Ctrl-R` | inline history search (fuzzy, 8 rows) |
| `Ctrl-Space` | ask the model now, without waiting for the pause |
| `Ctrl-Q` | at the prompt — add/remove `? ` at the start of the line; while a command runs — ask the model (see above) |

Source priority: history → bash-completion → model.
The model is not called for an empty line, nor for lines where history already
gave the exact continuation.

## Feedback about commands

We show only what carries information:

```
$ make test
...
 ✗ 2 · 14.2s          ← exit ≠ 0 or longer than 5 s; otherwise nothing
```

No marks for fast successful commands.

## Background jobs

Ordinary bash: `cmd &`, `jobs`, `fg`, `Ctrl-Z` work as always.
aish only adds:

- when a background job ends while you are at the prompt, one line above it:
  ` ✓ [1] cargo build --release · 2m13s` or
  ` ✗ [2] npm test · 41s · exit 1` (the command is cut to one line, time is
  counted from the moment aish saw the job). bash's own line `[1]+ Done …` is
  hidden by aish at that moment. How it works: an rc hook sends the job table
  (`jobs -l` in the C locale, a private OSC), aish checks the pids twice a second
  and, when a pid is dead, "pokes" bash (`bind -x`): it prints its own line and
  forgets the job, and `wait` gives the status. If the job ended while another
  command was running, bash prints its line as usual and ours is not printed
  (it would be a duplicate);
- `? what about job 2`: the job list goes into the context by itself, the model
  can call `jobs`/`job_output` (there is a log for jobs started by the agent
  with `background`).

For the model aish keeps its own job journal (pid, log, exit); you don't need it.

## Controlling aish

Inside a session the short command `ai` (and the always available long form
`aish`) has the same subcommands. The name `ai` always wins over anything of the
same name in the system; it can be changed or turned off in the config
(`command: mm` or `command: off`; `aish` works in any case). A bare `llm` is not
touched. `aish status` and `aish config` also work as ordinary CLI commands;
other arguments go to bash:

The command's arguments get a grey suggestion instantly and locally (no model,
higher priority than history): `ai ` → `status`; `ai c` → `hat`; `ai chat ` → a
model reference, preferably **not the current one** (the likely switch target),
then in config order; `ai o` → `n`/`ff` — the opposite of the current state.
To the right of the line there is a short dim explanation (`· now cloud/main`)
if there is room. `→` accepts, Tab shows a pager with all the options. Works for
`aish` and for the name from `command:`.

```
$ ai               ← status: providers, roles, availability (one line per role)
$ ai help          ← commands: `command — what it does`, long ones wrap (fits 36 columns); also `ai -h`
$ ai models        ← models by provider: alias, id (dim, cut with …), roles (`chat`, `complete`)
$ ai model chat ds ← switch a role's model AND save it to the config (`ai model ds` = chat; `ai chat ds` is the same)
$ ai profile       ← profiles (the active one is marked `*`); `ai profile inet` switches and saves
$ ai run [ask|judge|off]    ← may the model run commands (`--save` keeps it in the config)
$ ai tools [name on|off]   ← the agent's tools run/files/web (`--save` keeps it)
$ ai off / ai on          ← turn model suggestions off/on
$ ai config                ← puts `${EDITOR:-vi} ~/.config/aish/config.yaml` into the input line (Enter opens it)
```

A model reference is an `alias` (if it is unique) or `provider/alias`. Switching
a model edits only the role's value in the config (a targeted text patch:
comments, keys and order are not touched; the write is atomic, file permissions
are kept). If the file's shape is unusual, the model changes for the session
only, and a dim line says so. A role can be a list (`chat: {id: [mellum, ds]}`):
`ai model chat ds` puts the model **first**, the rest stay as fallbacks.

**Fallback models.** If a model fails to connect / times out / answers 5xx / 404
before the first token, the next one in the list is used, with one dim line
`↪ ds · mellum unavailable`. Errors after the answer has started are not retried.
A failed model is remembered for 60 s: the following requests go straight to the
working one (the same for `complete` suggestions, but silently). Secret masking
follows the model that answered.

**Profiles.** `profiles:` are named sets of roles, `profile:` is the active one
(none — the `roles:` section). With an active profile `ai model` edits the
profile's entry.

**Any line with `ai`.** A line starting with `ai` (or the name from `command:`)
is ours and never goes to bash or to the model. If the first word is exactly a
subcommand (`help`, `models`, `model`, `chat`, `complete`, `profile`, `status`,
`on`, `off`, `config`, `init`, `run`, `tools`), it runs. Anything else (a typo, text in another
language, any words) prints, dimmed: `unknown chst · did you mean chat?` (only if
a subcommand is close: edit distance ≤ 2 or a unique prefix), the line
`ask the model: ? <question>` and the ordinary `ai help`. A bare `ai` = `ai help`.
For typos in arguments: the same for an unknown model (`no model: x · did you
mean ds?`, otherwise `· have: a, b, …`), role and profile. `ai` and `aish` are not
highlighted red as unknown commands; with a typo in a subcommand a `→ chat`
suggestion appears on the right.

## Updating: `aish update`

`aish update` — for an install from the tarball/`install.sh` (`~/.local/bin`):
takes the latest release from GitHub, checks sha256 and atomically replaces the
binary, in one line: `aish 0.1.0 → 0.2.0` or `aish 0.2.0 — up to date` (`--check`
only reports). If the binary came from dpkg, it prints
`sudo apt update && sudo apt install --only-upgrade aish` and exits; it does not
run sudo itself.

A quiet hint: at most once a day, in the background (no network at startup, the
cache is in `~/.local/state/aish/update-check`), one dim line after a command
finishes:

```
 ↑ aish 0.2.0 — aish update
```

Without a config (first run) it stays silent: then only the first-run line
speaks. `update_check: off` in the config turns the check off.

## Installing: `aish init`

`./install.sh` (or `curl -fsSL https://aadegtyarev.github.io/aish/install.sh | bash`:
apt or tarball) builds and puts `aish` into `~/.local/bin`, then runs `aish init`
(in a session the same: `ai init`). No more than three questions, one line each,
Enter takes the default (it is dim):

```
Start aish: 1 — in every interactive bash, 2 — also in tmux, 3 — manually only [1]
config exists, keeping: ~/.config/aish/config.yaml          ← or model autodetection
  ~/.bashrc  +3 lines
Apply? [Y]
```

A block `# >>> aish >>>` … `# <<< aish <<<` is added to `~/.bashrc` (a rerun
replaces it, the original is in `~/.bashrc.aish-bak`, once): an interactive bash
with a terminal `exec`s aish by absolute path; the block sits at the very top of
`.bashrc`, so that the outer bash runs nothing (and prints nothing) before aish.
There is no loop: aish itself sets `AISH` while reading `.bashrc`;
`AISH_DISABLE=1 bash` is the escape hatch. `aish init --remove` removes the
blocks. Without a terminal the command only prints what it would do and exits
with code 1 (`--yes` applies the defaults). Keys from the environment are written
to the config as `$VAR`, never as a value. `chsh` is not called.

## First run

No config → aish probes `localhost:11434` (ollama), `localhost:1234`
(LM Studio), `$OPENAI_API_KEY` itself, writes a config and says so in one line:

```
aish · chat: cloud/gpt-6 · complete: local/qwen2.5-coder:1.5b · ai config — change
```

This is the only time aish speaks unasked. The line is cut to the width of the
terminal, and shown even if it arrives after the prompt is up.

## Session marker

In input mode, on the input line at the right edge (the last column minus one),
a dim `?` is a quiet sign that the model is near. It is hidden if the input (with
the suggestion) comes closer than 3 columns or wraps to another row, if the
prompt fills its row, and with `NO_COLOR`. It is temporary: on Enter the line is
redrawn without it, it is not in scrollback. The text is set by the host (in the
library — `RightHint`/`HintText`; `HintText::set` from any thread redraws the line
at once). States: `?` normally; `[N] ?` with N background jobs; `⋯` while a
suggestion request to the model takes longer than 300 ms (and stays on screen at
least 200 ms — no flicker).

## Narrow screens

The UI must work down to ~36 columns (a phone): the input line, ghosts, notices,
the `?` answer and the `Ctrl-Q` overlay don't break and don't run past the right
edge. The `?` marker on the right hides when there is no room for it (3 more free
columns after the input are needed). Long notices are cut to the width with `…`,
overlay hints get shorter. New UI elements are checked by tests at a narrow width
(`tests/narrow.rs`).

## Colors

| Element | Style |
|---|---|
| suggestion (ghost) | `dim` |
| the answer bar `│`, service lines | `dim` |
| `✗`, errors | red |
| `✓` | green |
| code/commands in the answer | accent (cyan by default) |
| **bold** in the answer | bold |

## Security

- Before sending to the cloud, secrets are masked: `*_KEY=`, `*TOKEN*`,
  `Authorization:`, private keys, passwords in URLs.
- For a local provider masking can be turned off.
- Nothing runs without Enter (or your `s`/`a` for the session), except allowlisted
  read-only commands. In `judge` mode the hard list still asks.
