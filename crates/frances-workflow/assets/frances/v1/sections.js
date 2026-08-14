// `frances:v1/sections` — transcript proxy + frame constructors.

import { WritableStream } from "whatwg:web-streams";

export const {
  transcript,
  MarkdownSection,
  ErrorSection,
  JsonSection,
  ReasoningSection,
  ToolUseSection,
  DiffSection,
  EntityRefSection,
} = globalThis.__frances_v1_stash__;

// Each writable-capable frame class composes a WHATWG WritableStream
// rather than subclassing one: a frame is a transcript entry with its
// own lifecycle, and conflating that with WritableStream's
// closed/errored/locked states would tangle two unrelated lifecycles.
// The shape mirrors TransformStream.writable — same instance on every
// access, so `frame.writable === frame.writable`.
//
// The Rust prototype's `write(delta)` is the underlying append op; we
// pull it off the prototype so the only public path is through the
// stream, and re-use it as the sink's `write` callback. The same
// trick lets us route the writable's close/abort hooks through
// `frame.close()` so a closing pipe seals the underlying frame —
// unless the workflow opted out via `frame.autoclose = false`.
function installWritable(cls) {
  const _append = cls.prototype.write;
  const _close = cls.prototype.close;
  delete cls.prototype.write;

  Object.defineProperty(cls.prototype, "autoclose", {
    value: true,
    writable: true,
    configurable: true,
  });

  const writables = new WeakMap();
  Object.defineProperty(cls.prototype, "writable", {
    configurable: true,
    get() {
      let w = writables.get(this);
      if (!w) {
        const frame = this;
        const autocloseFire = () => {
          if (frame.autoclose) {
            try {
              _close.call(frame);
            } catch (_) {
              // Frame already closed or never pushed — harmless.
            }
          }
        };
        w = new WritableStream({
          write(chunk) {
            _append.call(
              frame,
              typeof chunk === "string" ? chunk : String(chunk),
            );
          },
          close: autocloseFire,
          abort: autocloseFire,
        });
        writables.set(this, w);
      }
      return w;
    },
  });
}

installWritable(MarkdownSection);
installWritable(ReasoningSection);
