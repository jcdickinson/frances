# Tool-call accumulator cap (safety net)

Cap `ToolCallAccumulator::in_progress.len()` in `crates/frances-llm/src/providers/openai/tool_calls.rs`. On `Start`, if accepting it would push the map past N (8? 16?), return a new `ToolCallError::TooMany { index, cap }`. The `?` in `crates/frances-llm/src/providers/openai/mod.rs:229` already propagates that out as a stream error.

## Motivation

A misbehaving model (observed: `google/gemini-2.5-flash-lite` with `tool_choice: Required` and two tools) can stream hundreds of distinct `index` values in one response. The accumulator's `BTreeMap` is unbounded, so memory and trace volume grow until the provider's `reqwest` total timeout (`stream_idle_timeout_ms`, default 120 s) finally cuts the request. The same misbehavior also blows up `parse_outcome`'s `many` branch in `crates/frances-daemon/src/server/auto_judge.rs`, so the auto-judge wastes ~2 min per permission gate before falling through to the user.

## Why this is the fallback, not the fix

The intended fix is **stream-finalize on index advance** — once a `Start` at `index N` arrives, the call at `index N-1` is provably done (OpenAI-style providers serialise tool calls in-stream), so we can emit a `StreamEvent::ToolCall` and drop the older entries from the map. Memory becomes O(1) regardless of how many calls the model streams, and callers can react incrementally (e.g. auto-judge cancels the stream once it sees `n > 1`).

That redesign also depends on **real cancellation** being wired end-to-end (JS `AbortSignal` → Rust task), which today is cosmetic: `chat.js`'s `onAbort` only errors the `ReadableStream`, while the underlying `tokio::spawn` in `crates/frances-workflow/src/modules/chat.rs:454` keeps draining the provider until it ends naturally.

If stream-finalize + cancellation lands and behaves, the cap is unnecessary. Keep this TODO around as the belt-and-braces fallback for runaway models we haven't anticipated.

## Sketch

```rust
const TOOL_CALLS_PER_RESPONSE_CAP: usize = 16;

ToolCallEvent::Start { id, name } => {
    if self.in_progress.contains_key(&delta.index) {
        return Err(ToolCallError::AlreadyStarted(delta.index));
    }
    if self.in_progress.len() >= TOOL_CALLS_PER_RESPONSE_CAP {
        return Err(ToolCallError::TooMany {
            index: delta.index,
            cap: TOOL_CALLS_PER_RESPONSE_CAP,
        });
    }
    self.in_progress.insert(delta.index, ToolCallBuilder { ... });
}
```

Pick a cap generous enough that real parallel-tool-call bursts never trip it. 16 is well above anything a sane workflow would surface; the runaway case observed was 800+.
