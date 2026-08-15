// `frances:v1/messages` — chat-message entities (kind "chat").
//
// Snapshot-only producer: `{ source, text }` where source is "user",
// "assistant", "reasoning", or "internal". Streaming updates the
// snapshot with the full accumulated text on every write; there is no
// entity stream. The frontend's chat components render `snapshot.text`
// directly, live or settled, so a forced settle after a crash loses
// nothing.

import { WritableStream } from "whatwg:web-streams";
import { createEntity } from "frances:v1/entities";
import { transcript, EntityRefSection } from "frances:v1/sections";

// One-shot message: create, reference from the transcript, settle.
export function postMessage({ source = "internal", content }) {
  const snapshot = { source, text: content };
  const handle = createEntity("chat", snapshot);
  transcript.push(new EntityRefSection({ id: handle.id }));
  handle.settle(snapshot);
}

// Streaming message. Returns `{ write(delta), writable, close() }`;
// `writable` composes a WHATWG WritableStream over the same sink so a
// finished (or aborted) pipe settles the message.
export function openMessage(source = "internal") {
  const handle = createEntity("chat", { source, text: "" });
  transcript.push(new EntityRefSection({ id: handle.id }));

  let text = "";
  let settled = false;
  const close = () => {
    if (settled) return;
    settled = true;
    handle.settle({ source, text });
  };
  const write = (delta) => {
    text += typeof delta === "string" ? delta : String(delta);
    handle.updateSnapshot({ source, text });
  };

  return {
    write,
    close,
    writable: new WritableStream({
      write(chunk) {
        write(chunk);
      },
      close,
      abort: close,
    }),
  };
}
