// `frances:v1/approval` — ask the user a yes/no permission question.
//
// One async function. Returns one of:
//   - { type: "yes", details: string | null }
//   - { type: "no",  details: string | null }
//
// The caller composes the prompt; tools that want a reusable prompt
// rendering keep it as a method on themselves and call it before
// `approve()`. `toolCall` is optional — a workflow may gate something
// that isn't a tool call (a plain "are you sure?" confirmation, a
// policy check, etc). `allowAuto` flags the gate as eligible for the
// host's auto-approver; nothing acts on it yet.

const { _approve } = globalThis.__frances_v1_stash__;

export async function approve(options) {
  if (options === null || typeof options !== "object") {
    throw new TypeError(
      "approve: expected an options object: { prompt, toolCall?, allowAuto? }",
    );
  }
  if (typeof options.prompt !== "string") {
    throw new TypeError("approve: `prompt` must be a string");
  }
  if (options.toolCall !== undefined && options.toolCall !== null) {
    const tc = options.toolCall;
    if (typeof tc !== "object") {
      throw new TypeError(
        "approve: `toolCall` must be an object: { id, name, arguments }",
      );
    }
    if (typeof tc.id !== "string") {
      throw new TypeError("approve: `toolCall.id` must be a string");
    }
    if (typeof tc.name !== "string") {
      throw new TypeError("approve: `toolCall.name` must be a string");
    }
    // `arguments` is a JSON-shaped any; the Rust side recursively
    // converts whatever it gets.
  }
  if (
    options.allowAuto !== undefined &&
    options.allowAuto !== null &&
    typeof options.allowAuto !== "boolean"
  ) {
    throw new TypeError("approve: `allowAuto` must be a boolean if provided");
  }
  return await _approve(options);
}
