# Backlog and ideas

## Backlog (agreed, in order)

1. **Memory file.** `~/.config/aish/memory.md`, limit in config
   (`memory: {limit: 2000 chars}` or tokens); tools `memory_read` and
   `memory_write` (replace whole); over the limit the write is refused with
   `now N, limit M` so the model rewrites it shorter.
2. **Request order for provider caching.** Static system prompt → memory →
   skills index → summary + conversation → dynamic context (cwd, git, recent
   blocks, jobs) last.
3. **Overridable system prompt.** `system: {replace: <file|text>, append: <text>}`.

## Ideas

Not planned yet; each needs a design pass before work starts.

## Remote: aish on servers over ssh

On `ssh host`, offer to put aish there (`Enter` this session · `i` install ·
`n` no · `N` never). One ControlMaster connection for auth, the binary from the
GitHub release (checked against SHA256SUMS), model requests tunnelled back to
the laptop's configured models over a forwarded unix socket (keys never leave
the laptop; only configured models, no general proxy). `aish get`/`aish put` for
files and "open this URL on the laptop" over the same channel.

## Skills: small, composable, cheap in context

Skills are short notes the model can pull in for a task (how to use a
project's deploy script, how this machine's backups work, house style for
commit messages). Built for local models with small context windows first.

**Format, hard limits.** One file per skill, plain Markdown with a tiny header:

```markdown
---
name: deploy-staging          # kebab-case, unique
when: deploying, staging, release to test   # trigger words, ≤ 12
---
Body: imperative steps and facts. No prose, no examples unless essential.
```

- Body ≤ 1 500 characters (~400 tokens); header `when` ≤ 120 characters.
  aish refuses to load a skill over the limit and says which one.
- Surface: a skill may mention commands but cannot grant anything. Running
  them goes through the normal permission rules. No includes, no templating,
  no code.

**Discovery and trust.** `~/.config/aish/skills/*.md` (personal) and
`.aish/skills/*.md` in the current repo. A file that passes the format check
(header, limits) is trusted: a skill is only text within hard limits and
grants nothing, so users can write and share their own freely. `aish skills`
lists them with sizes; `aish skills new <name>` opens a stub.

**Import.** `aish skills import <github url>` (a file, a directory or a whole
repo of skills): fetch over HTTPS, validate every file against the format,
show the list (`name — when · size`) and install on Enter into
`~/.config/aish/skills/<source>/`. `aish skills update` re-fetches imported
sources; `aish skills rm <name|source>` removes them.

**Loading into context.** Never all bodies at once.

1. Every turn the model sees only an index: `name — when` per skill, capped
   (for example the 20 best matches, ≤ 600 characters in total).
2. Ranking for the index is local and cheap: word overlap between the
   question, cwd/git context and `when` (+ name), no embeddings needed.
   Embeddings only if a local embedding model is configured.
3. Bodies arrive in one of two ways. The top 1–2 matches above a threshold
   are attached directly, within a budget relative to the model's
   `context_window` (for example ≤ 10 %). The model can also call
   `read_skill(name)` for others. The dim line `◆ deploy-staging` shows what
   was loaded.
4. Small-context models (`context_window` < 16k) get the index only plus at
   most one auto-attached body.

**First skill: how to write skills.** Ships built-in and is used by
`aish skills new`. It covers the format, the limits, one fact per line,
imperative mood, triggers that are words users actually type, and splitting
instead of growing. The model can draft a skill from the current conversation
("save this as a skill"); the user reviews it before it is written.

Open questions: versioning when a skill goes stale; conflicts between two
skills with the same name from different sources.
