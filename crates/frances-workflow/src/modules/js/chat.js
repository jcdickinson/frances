// `frances:v1/chat` — `ChatSession` constructor.
//
// `stream({ signal })` returns a `StreamingResponse`:
//   - `events`:    ReadableStream<StreamEvent> — raw provider events.
//   - `text`:      ReadableStream<string>      — text deltas only,
//                                                 lazily derived from
//                                                 `events`. Reading
//                                                 either locks the
//                                                 other (WHATWG
//                                                 pipeThrough locks
//                                                 its source).
//   - `completed`: Promise<{ text, usage }>    — resolves when the run
//                                                 settles, regardless of
//                                                 whether the streams
//                                                 were read.
//
// `signal` is an optional AbortSignal; firing it errors the events
// stream with the signal's reason. The underlying Rust task keeps
// draining its channel until the LLM run ends — full cancellation
// to the provider is a follow-up.
//
// The raw async-iterable returned by the Rust side is captured into
// closure here (`_innerStream`) and never escapes: the stash entry
// it came from is deleted by the host after this module evaluates.

import { ReadableStream, TransformStream } from "whatwg:web-streams";

const __s = globalThis.__frances_v1_stash__;
const ChatSession = __s.ChatSession;
const _innerStream = __s.__chat_inner_stream;

ChatSession.prototype.stream = async function stream({ signal } = {}) {
  const inner = await _innerStream.call(this);

  let streamController;
  const events = new ReadableStream({
    start(c) {
      streamController = c;
    },
    async pull(controller) {
      const { done, value } = await inner.events.next();
      if (done) controller.close();
      else controller.enqueue(value);
    },
  });

  if (signal) {
    const onAbort = () => {
      try {
        streamController.error(signal.reason);
      } catch (_) {
        // Stream already closed or errored — nothing to do.
      }
    };
    if (signal.aborted) onAbort();
    else signal.addEventListener("abort", onAbort);
  }

  let textStream;
  return {
    events,
    get text() {
      // `pipeThrough` locks `events`, so accessing `r.text` and then
      // `r.events.getReader()` (or vice versa) will throw per WHATWG
      // spec — text and events are alternative views of one source.
      if (!textStream) {
        textStream = events.pipeThrough(
          new TransformStream({
            transform(ev, controller) {
              if (ev.type === "text") controller.enqueue(ev.delta);
            },
          }),
        );
      }
      return textStream;
    },
    completed: inner.completed,
  };
};

export { ChatSession };
