# Plugins and modules

Two layers, both compile-time Rust traits today.

## Editor plugins (`crates/llmsh-editor`, crate `lineedit-wip`)

`Editor::builder().with(plugin)...build()`. A `Plugin` has a `name`, a
`priority` (higher first) and optional hooks:

| Hook | When | Notes |
|---|---|---|
| `on_key(ctx, key) -> Handled` | every key, before the keymap | `Yes` owns the key (modal UIs: Ctrl-R, the Tab pager) |
| `on_action(ctx, name)` | a key bound to a named action fired | actions are declared in `bindings()`; users rebind via `Keymap::bind_action` |
| `on_buffer_change(ctx)` | line text changed | |
| `suggest(ctx) -> Option<&str>` | each redraw, cursor at end | ghost text; first plugin (by priority) with a hit wins |
| `highlight(text, out)` | each redraw | layers; higher priority paints over lower |
| `decorate(ctx, width, out)` | each redraw | rows below the input, an alternative input line (search query), or `right_hint` (transient dim text at the right edge, only if it fits) |
| `on_submit(ctx, line) -> Submit/Intercept/Edit` | Enter | all plugins consulted; any `Intercept` makes `Event::Intercepted` |
| `on_delivery(ctx, payload)` | a result arrived through the plugin's `Handle` | only if the line did not change since the request |

Plugin offsets are clamped to character boundaries (`Ctx`), and the host runs the editor under `catch_unwind`. Plugins must not block. Slow work (completion sidecar, a model) runs elsewhere
and calls `Handle::deliver(ctx.generation(), payload)`; the editor drops
results whose generation is stale and the host's `waker` is called so it can
`Editor::poll`. Built-ins are plugins too: `HistorySuggest`, `HistorySearch`,
`CompletionPager` (takes any `CompletionSource`), `SyntaxHighlight`, `Intercept`.

## llmsh modules (`src/module.rs`, `src/modules/`)

`trait Module` hooks: `editor_plugins(env)` (once, returns plugins),
`on_block_finished(block)`, `on_prompt()`, `on_intercept(line, out, task)` (a slow reply takes the `Task`: a worker thread streams rows to the terminal and calls `finish(prefill)`; meanwhile keys are buffered and Ctrl-C sets the task's cancel flag).
Built-ins: `history`, `highlight`, `completion` (bash sidecar), `model`
(`?` questions, `llmsh` command, model suggestions). Config `modules: [history, completion]` selects a
subset; absent means all.

Lifecycle: startup (config, modules build editor plugins) -> output stream
(OSC 133 B + 10 ms quiet = prompt shown, `on_prompt`, editor takes the line)
-> Enter (`on_submit` hooks; claimed lines go to `on_intercept`, others are
typed into bash) -> command runs -> OSC 133 D (`on_block_finished`).

## Planned: out-of-process plugins (design note, not implemented)

JSON-RPC 2.0 over stdio, LSP-style framing (`Content-Length`). The hooks above
map to requests/notifications with plain data: `suggest`/`highlight`/`decorate`
are requests carrying `{text, cursor, generation}` and returning
suggestions/spans/rows; `on_key`, `on_buffer_change`, `on_submit`,
`on_block_finished` are notifications or requests; async answers are
notifications tagged with the `generation` they were computed for, which maps
onto `Handle::deliver`. A proxy `Plugin`/`Module` implementation per process
keeps the in-process API unchanged.
