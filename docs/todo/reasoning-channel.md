Reasoning tokens are streamed as `StreamEvent::TextDelta` so the TUI
doesn't freeze on thinking models, which means a thinking-only turn
renders the same as a normal response — same block, same colour, no
separator.

The Rust side keeps reasoning *out* of `CompletionOutcome.text`, so chat
history stays clean; but the JS `pipeAssistantTextToFrame` sink can't tell
the difference, so reasoning ends up in the scrollback DB and in
`recordStepTranscript` (which later gets summarised back into the model).

To distinguish properly:

- add `StreamEvent::ReasoningDelta(String)` to `frances-models-llm/wire.rs`
- emit it from `providers/genai/mod.rs` on the
  `ChatStreamEvent::ReasoningChunk` arm, instead of folding into
  `TextDelta`
- plumb through `chat.rs` → `JsStreamEvent` as a new `type: "reasoning"`
  event, and through `chat.js`'s text TransformStream so `r.text` stays
  response-only
- give the workflow a separate frame / sender (e.g. greyed-out
  `"thinking"` block) so reasoning is visually distinct and not captured
  in the step transcript that gets summarised back to the model
