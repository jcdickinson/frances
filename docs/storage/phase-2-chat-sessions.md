# Phase 2 — Chat sessions

Honor an `ephemeral: true` flag on `new ChatSession({...})`. Ephemeral
sessions never write to `chat_sessions` / `chat_messages` and never read
prior turns from them — their entire lifecycle is in-process.

## Current state

- Persistence shape (`crates/frances-daemon/src/history/migrations/0001_init.sql`):
  - `chat_sessions` row per session (opaque `session_id`, `model_intents`
    JSON, `created_at`).
  - `chat_messages` rows for primitives (`user`/`assistant`/`tool_call`/
    `tool_result`) and tagged provider history payloads.
  - `primary_chat_session` append-only log; the bottom-of-stack chat
    references it.
- JS surface (`crates/frances-workflow/src/modules/js/chat.js`):
  - `new ChatSession({ model_intents })` — only field parsed by Rust.
- Rust ctor (`crates/frances-workflow/src/modules/chat.rs` →
  `frances_models_llm::chat::ChatSessionBuilder`):
  - `ChatSessionBuilder { model_intents: Vec<String> }`.
- The runtime ignores any other key the JS object carries (rquickjs
  reads only what we ask for). So passing `ephemeral: true` today is
  silently dropped — that's the bug.

For comparison, `ChatSessionManager::complete(...)` is already a
non-persisted one-shot. It does not satisfy the JS use case because it
has no `push` queue, no tool-loop, no `stream()` surface — workflows
need the full `ChatSession` API for transient sessions too.

## Desired state

```js
// persisted today's way
const real = new ChatSession({ model_intents: ["chat"] });

// transient: never touches the DB
const scratch = new ChatSession({ model_intents: ["classify"], ephemeral: true });
```

Behaviorally, ephemeral sessions:

- Skip `ensure_row`. No `chat_sessions` insert. `id()` always returns
  `None`.
- Skip `append_primitive*`. The `pending` queue still drains so `run`
  has its inputs, but nothing is written.
- Skip `append_history`. Provider-history payloads stay in memory only.
- `loaded_history` returns empty. The provider sees only the in-memory
  `pending` drain — i.e. exactly what JS has pushed since the last
  `stream()` (or since this session was created).
- Otherwise identical: tool calls, streaming events, AbortSignal, etc.

Persisted (default) sessions keep today's behavior verbatim.

## Tasks

1. `frances-models-llm::chat::builder`: add `ephemeral: bool` to
   `ChatSessionBuilder`. Default `false`. Builder method
   `with_ephemeral(bool)`.
2. `frances-workflow/src/modules/chat.rs::parse_intents` (or rename to
   `parse_chat_options`): read `ephemeral` from the arg object as a
   bool; default `false` if absent. Pass through to the builder.
3. `frances-llm/src/chat/session.rs::ChatSession`: store `ephemeral`
   alongside `id`. Branch in `ensure_row` / `run`:
   - `ensure_row`: if `ephemeral`, return early without DB write. The
     existing `id: Mutex<Option<_>>` stays `None`. Callers that need an
     id should check first; today only `manager::primary` calls it
     explicitly.
   - `run`: when `ephemeral`, skip `append_primitive` over `drained`,
     skip `loaded_history` (substitute an empty `Vec`), skip
     `append_history` and the post-run `append_primitive_assistant` /
     `_tool_call` calls. Still pass `new_inputs` to the provider as
     today.
4. `manager::primary`: ephemeral sessions cannot be primary. Make
   `primary(builder)` reject `builder.ephemeral`, or just document that
   only `create()` honors the flag (primary always persists).
5. Touch up the JS docstring header in `chat.rs` and `js/chat.js` so the
   `ephemeral` option is in the contract.
6. Test:
   - In-tree workflow test: ephemeral session runs two rounds, second
     round's `loaded_history` is empty, no rows in `chat_sessions`.
   - Persisted session still writes the same rows as today.

## Open questions

- Should an ephemeral session refuse `system` after the first `user`
  push? Today persisted sessions do (the `system_locked` flag). Keep
  the same rule — it's about the model contract, not persistence.
- Tool-result handling for an ephemeral session that survives multiple
  `stream()` rounds — the JS layer pushes `tool` messages back onto
  `chat` between rounds. Without `loaded_history`, the next round will
  see those via `new_inputs` (the `pending` drain). That's fine, but
  needs a test to lock it in: the second round must include the tool
  result from the first.
- delete `primary_chat_session`.

## Definition of done

- `new ChatSession({ model_intents: [...], ephemeral: true })` runs a
  full stream-loop with tool calls and leaves zero rows in
  `chat_sessions` or `chat_messages` for that session.
- Existing persisted tests pass unchanged.
- `import.meta.args`-style docs reference `ephemeral` in the JS API
  comment.
