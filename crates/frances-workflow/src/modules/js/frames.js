// `frances:v1/frames` — transcript proxy + frame constructors.

import { WritableStream } from "whatwg:web-streams";

export const { transcript, MarkdownFrame, ErrorFrame, JsonFrame } =
  globalThis.__frances_v1_stash__;

// MarkdownFrame composes a WHATWG WritableStream rather than subclassing
// one: a frame is a transcript entry with its own lifecycle (sealed when
// the next frame is pushed), and conflating that with WritableStream's
// closed/errored/locked states would tangle two unrelated lifecycles.
// The shape mirrors TransformStream.writable — same instance on every
// access, so `frame.writable === frame.writable`.
//
// The Rust prototype's `write(delta)` is the underlying append op; we
// pull it off the prototype so the only public path is through the
// stream, and re-use it as the sink's `write` callback.
const _append = MarkdownFrame.prototype.write;
delete MarkdownFrame.prototype.write;

const writables = new WeakMap();
Object.defineProperty(MarkdownFrame.prototype, "writable", {
  configurable: true,
  get() {
    let w = writables.get(this);
    if (!w) {
      const frame = this;
      w = new WritableStream({
        write(chunk) {
          _append.call(frame, typeof chunk === "string" ? chunk : String(chunk));
        },
      });
      writables.set(this, w);
    }
    return w;
  },
});
