// `frances:v1/chat` — `ChatSession` constructor.
//
// Constructor:
//   new ChatSession({ model_intents, ephemeral? })
//
//   - `model_intents` (string[], required): config keys walked in order
//     when resolving the model for each call.
//   - `ephemeral` (bool, optional, default false): when true, the
//     session never reads or writes `chat_sessions`/`chat_messages`.
//     The provider sees only what was pushed since the last stream().
//
// `stream({ signal, maxToolCalls })` returns a `StreamingResponse`.
// `maxToolCalls` is an optional non-negative integer cap on the number
// of tool calls retained from this round; further calls are dropped at
// the wire and the stream is closed gracefully (returns `Ok` with the
// first `maxToolCalls`). Useful to bound runaway models.
//
//   - `events`:    ReadableStream<StreamEvent> — raw provider events.
//   - `text`:      ReadableStream<string>      — text deltas only,
//                                                 lazily derived from
//                                                 `events`. Reading
//                                                 either locks the
//                                                 other (WHATWG
//                                                 pipeThrough locks
//                                                 its source).
//   - `completed`: Promise<{ text, tool_calls, usage }> — resolves once
//                                                 the LLM round has
//                                                 settled, every initial
//                                                 handler has run, all
//                                                 results have been
//                                                 pushed to chat history,
//                                                 AND any post-batch
//                                                 turns registered via
//                                                 `scope.lock` have run
//                                                 to completion.
//   - `signal`:    AbortSignal | undefined      — passthrough of the
//                                                 caller's signal.
//
// ChatSession does one LLM round-trip per `stream()` and dispatches the
// tool calls that come back. A workflow drives the multi-round loop by
// calling `stream()` until `tool_calls` comes back empty.
//
// Lifecycle of one `stream()` call:
//   1. Send the provider request, receive tool_calls.
//   2. For each tool_call: spawn its handler in parallel. Each handler
//      gets a fresh `scope` overlay.
//   3. After every handler settles, push the initial tool_results to
//      chat history in `tool_calls` order.
//   4. Run any post-batch turns registered via `scope.lock(fn)` in
//      finish order (the order their handlers returned).
//   5. Resolve `completed`.
//
// `scope` (per-handler):
//   - `tools`     read-only view of `chat.tools`.
//   - `push`      forwards a message to chat history.
//   - `stream`    drives another round on the same chat (additional turns
//                 land in the same shared history).
//   - `toolCall`  settable middleware hook for nested rounds.
//   - `lock(fn)`  register a post-batch turn for this slot. Fire-and-
//                 forget; the handler still returns its own tool_result.
//
// The raw async-iterable returned by the Rust side is captured into
// closure here (`_innerStream`) and never escapes: the stash entry it
// came from is deleted by the host after this module evaluates.

import { ReadableStream, TransformStream } from "whatwg:web-streams";

const { ChatSession, __chat_inner_stream: _innerStream } =
  globalThis.__frances_v1_stash__;

ChatSession.prototype.stream = function stream(opts) {
  return _streamWithDispatch(this, opts, () => this.toolCall);
};

// One provider round-trip + dispatch of the resulting tool calls + any
// registered post-batch turns. `getHook` is a thunk so callers see the
// latest value of the hook even if it's mutated between `stream()` being
// called and `completed` being awaited.
async function _streamWithDispatch(chat, opts, getHook) {
  const signal = opts && opts.signal;
  const maxToolCalls = opts && opts.maxToolCalls;
  const inner = await _innerStream.call(chat, { maxToolCalls });

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
      // Fire Rust cancel first — that drops the in-flight HTTP request
      // so the provider stops generating. Erroring the local stream
      // only unblocks JS-side readers.
      try {
        inner.cancel.fire();
      } catch (_) {}
      try {
        streamController.error(signal.reason);
      } catch (_) {
        // Stream already closed or errored — nothing to do.
      }
    };
    if (signal.aborted) onAbort();
    else signal.addEventListener("abort", onAbort);
  }

  const completed = (async () => {
    let raw;
    try {
      raw = await inner.completed;
    } catch (err) {
      // The Rust side rejects with the `__cancelled__` sentinel on
      // `ChatError::Cancelled`. Either way, if the user aborted, the
      // user-visible rejection should match `events`/`text`: throw
      // `signal.reason`, not the internal error string.
      if (signal && signal.aborted) throw signal.reason;
      throw err;
    }
    const hook = getHook();
    const session = {
      slots: raw.tool_calls.map(() => ({
        result: undefined,
        finishedAt: -1,
        turn: null,
      })),
      nextFinishIdx: 0,
    };

    await Promise.all(
      raw.tool_calls.map((call, idx) =>
        _dispatchSlot(chat, call, hook, session, idx),
      ),
    );

    // Push initial tool_results in tool_calls order.
    for (let i = 0; i < session.slots.length; i++) {
      chat.push(session.slots[i].result);
    }

    // Run registered turns in finish order.
    const turns = session.slots
      .map((slot, idx) => ({ slot, idx }))
      .filter((e) => e.slot.turn !== null)
      .sort((a, b) => a.slot.finishedAt - b.slot.finishedAt);
    for (const e of turns) {
      try {
        await e.slot.turn();
      } catch (err) {
        // Turn fn threw — surface a synthetic user message so the
        // conversation can continue, but don't crash the stream.
        chat.push({
          role: "user",
          content:
            `(internal: post-batch turn for tool_call ${raw.tool_calls[e.idx].id} threw: ` +
            `${String((err && err.message) || err)})`,
        });
      }
    }
    return raw;
  })();

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
    completed,
    signal,
  };
}

// Resolve one tool call to a `{ role: "tool", ... }` message. Handler
// errors and missing-tool cases collapse to an `is_error: true` result
// so the loop never crashes. Records finish order on the slot so the
// post-batch turn loop can sort by it.
async function _dispatchSlot(chat, call, hook, session, idx) {
  const scope = _createScope(chat, session, idx);
  const invoke = async () => {
    const tool = chat.tools.find((t) => t.name === call.name);
    if (!tool) return _errorResult(call.id, `tool not found: ${call.name}`);
    return await tool.handler({ call, scope });
  };
  let result;
  try {
    result = hook ? await hook({ call, invoke }) : await invoke();
  } catch (err) {
    result = _errorResult(call.id, String((err && err.message) || err));
  }
  session.slots[idx].result = result;
  session.slots[idx].finishedAt = session.nextFinishIdx++;
}

function _errorResult(call_id, content) {
  return { role: "tool", call_id, content, is_error: true };
}

// `scope` mirrors the dispatch surface for the duration of a tool
// handler. `tools` is the parent chat's array (read-only). `push` and
// `stream` forward to the chat; `stream`'s dispatch uses this scope's
// `toolCall` hook so handlers can gate nested rounds. `lock(fn)` defers
// `fn` to run in the post-batch turn loop.
function _createScope(chat, session, idx) {
  let toolCall;
  return {
    get tools() {
      return chat.tools;
    },
    push(message) {
      chat.push(message);
    },
    get toolCall() {
      return toolCall;
    },
    set toolCall(fn) {
      toolCall = fn;
    },
    stream(opts) {
      return _streamWithDispatch(chat, opts, () => toolCall);
    },
    lock(fn) {
      if (session.slots[idx].turn !== null) {
        throw new Error(
          "scope.lock: already registered a turn for this slot",
        );
      }
      session.slots[idx].turn = fn;
    },
  };
}

export { ChatSession };
