// `frances:v1/approval` — ask the user a yes/no/chat question.
//
// One async function for now. Returns one of:
//   - { type: "yes",  details: string | null }
//   - { type: "no",   details: string | null }
//   - { type: "chat", content: string }
//
// The prompt is forward-looking: callers pass a string today, but the
// surface is shaped so multi-choice and richer prompts can land without
// touching call sites.

const { _approve } = globalThis.__frances_v1_stash__;

export async function approve(prompt) {
  if (typeof prompt !== "string") {
    throw new TypeError("approve: expected a string prompt");
  }
  return await _approve(prompt);
}
