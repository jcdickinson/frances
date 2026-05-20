OpenRouter's `delta.reasoning` (kimi-k2.6, deepseek-r1, …) is currently
folded into the same `StreamEvent::TextDelta` stream as `delta.content`,
so reasoning and the actual response render identically in the TUI —
same block, same colour, no separator. It's just enough to stop the UI
freezing while a thinking model chews through tokens, but visually a
thinking-only turn looks the same as a normal response.

The Rust accumulator keeps reasoning *out* of `text` so chat history /
`CompletionOutcome.text` stay clean, but the JS `pipeAssistantTextToFrame`
sink doesn't know the difference — reasoning ends up persisted in the
scrollback DB and in `recordStepTranscript`, and it gets fed back into
later summary turns.

To distinguish properly:

- add `StreamEvent::ReasoningDelta(String)` to `frances-models-llm/wire.rs`
- emit it from `providers/openai/mod.rs` instead of re-using `TextDelta`
- plumb through `chat.rs` → `JsStreamEvent` as a new `type: "reasoning"`
  event, and through `chat.js`'s text TransformStream so `r.text` stays
  response-only
- give the workflow a separate frame / sender (e.g. greyed-out
  `"thinking"` block) so reasoning is visually distinct and not captured
  in the step transcript that gets summarised back to the model
