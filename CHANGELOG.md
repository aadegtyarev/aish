# Changelog

All notable changes to aish are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

New entries go under **Unreleased**. At release time that heading becomes the
version and date, and the release notes on GitHub are taken from it.

## [Unreleased]

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

[Unreleased]: https://github.com/aadegtyarev/aish/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/aadegtyarev/aish/releases/tag/v0.1.0
