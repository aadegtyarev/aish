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
  (`OSC 1337;llmsh;t=<plain>` or `b=<base64>`, stripped before reaching the terminal),
  `D;<exit>`, and the status line (` ✗ 101 · 14.2s`, ` ✓ 14.2s` only for exit != 0 or > 5 s).
  OSC 133 and OSC 7 are forwarded as-is.
- Command text comes from `history 1`; commands skipped by `HISTCONTROL`/`set +o history`
  are recorded with an empty command.
