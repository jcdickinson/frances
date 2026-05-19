# Eager tool-call events (stream-finalize on index advance)

Today the OpenAI-style provider accumulates every tool call in `ToolCallAccumulator::in_progress` (a `BTreeMap<index, ToolCallBuilder>`) for the whole response and only emits `StreamEvent::ToolCall`s after `finalize()` at end-of-stream (`crates/frances-llm/src/providers/openai/mod.rs:238-246`). The consolidated `StreamEvent::History` is emitted in the same shot. Consumers therefore see tool calls only as a batch, after the stream is done.

Change this to **emit each tool call as soon as the next index advances** (and the last one at end-of-stream). OpenAI-style providers serialise tool calls in the stream — all `index: 0` fragments arrive before any `index: 1` fragment — so once a `Start` at `index N` arrives, the call at `index N-1` is provably complete. Parse its arguments, emit `StreamEvent::ToolCall`, drop it from the map.

## Why

- **Memory becomes O(1)** in the number of tool calls regardless of how many the model streams. Today a runaway model (see [`tool-call-cap.md`](tool-call-cap.md) for the observed `gemini-2.5-flash-lite` case) lets the map grow unbounded for the whole 2-minute timeout window.
- **Callers can react incrementally.** The auto-judge in `crates/frances-daemon/src/server/auto_judge.rs` already knows that "two tool calls" is malformed; with eager events it can decide that on the second call and cancel the stream instead of waiting it out.
- **The `StreamEvent::ToolCall` event already exists** and is wired through `crates/frances-workflow/src/modules/chat.rs`. The change is when it fires, not what it carries.

## Depends on cancellation landing first

The eager-emit redesign only pays off if the *consumer* can stop the stream when it has seen enough. Today cancellation is cosmetic:

- `chat.js`'s `onAbort` calls `streamController.error(signal.reason)`, which errors the JS-side `ReadableStream`.
- The underlying `tokio::spawn` in `crates/frances-workflow/src/modules/chat.rs:454` keeps draining the provider; the `bytes.next().await` loop in `crates/frances-llm/src/providers/openai/mod.rs:200` has no cancellation hook.
- The event channel is `mpsc::unbounded_channel`, so JS-side backpressure doesn't push back either.

So **fix cancellation first**:

1. Plumb a `CancellationToken` (or `oneshot` abort handle) into `start_stream` so JS's `onAbort` can fire it.
2. Pass it down through `ChatSession::run` into the provider's `stream()` impl.
3. Wrap the `bytes.next().await` SSE drain in `tokio::select!` against the token.
4. Audit the mpsc — once we have early cancellation, an unbounded queue is less dangerous, but a bounded channel would make the design self-correcting.

Once those land, do the eager-emit change in `tool_calls.rs` and `mod.rs`, then teach the auto-judge to cancel on its second `StreamEvent::ToolCall`.

## Wire-contract notes

- Today: `StreamEvent::History(consolidated)` is emitted once at end-of-stream, then all `StreamEvent::ToolCall`s in order, then the future resolves. A few consumers may rely on "all tool_calls arrive after History".
- After: `StreamEvent::ToolCall`s arrive in-stream as each one finalises; the consolidated `StreamEvent::History` still fires at end-of-stream as today, so the cache primitive contract is preserved.
- Search for `StreamEvent::ToolCall` and `StreamEvent::History` consumers before changing the order; the workflow chat module (`crates/frances-workflow/src/modules/chat.rs`) is the main one.

## When the cap TODO goes away

If this lands cleanly, [`tool-call-cap.md`](tool-call-cap.md) becomes optional belt-and-braces. Keep it on the list until eager-emit + cancellation are both shipped and observed to handle real runaway-model traffic.
