# Deterministically repair qwen3-coder tool-call args

## The quirk

The qwen3-coder family (both base and `-next`) only emits **scalar** tool-call
arguments. When a tool's schema declares a non-scalar parameter (object or
array), qwen serialises that value to a **JSON string** and passes it as a
string arg instead of the structured value. It's a serving-stack quirk, not a
prompt problem — it shows up regardless of prompting.

Example: a tool with `parameters: { paths: { type: "array", ... } }` gets a
call whose `arguments.paths` is `"[\"a\",\"b\"]"` (a string) rather than
`["a", "b"]`.

## Why it matters now

We added Rust-side tool-call validation against each tool's JSON schema
(`frances-models-llm` `tool_args::validate`, wired through the chat APIs). With
qwen, a deep-JSON arg arrives as a string, so validation will (correctly) reject
it as a type mismatch → the model gets an error result every time it uses such a
tool. Until the repair lands, qwen is effectively degraded for non-scalar tools.

## The fix

A deterministic, pre-validation repair: for each tool-call argument whose
**schema type is `object` or `array`** but whose **value is a string**, attempt
to `serde_json::from_str` the string and substitute the parsed value. If it
parses and then validates, the call proceeds with clean args; if not, fall
through to the normal validation error.

Best placed at the **genai provider boundary** (where we know the model/provider
and already hold the tool schemas in `ProviderRequest`) so every consumer —
streaming, `complete`, `complete_enforced` — gets clean args transparently. Keep
it schema-driven (only coerce where the schema expects non-scalar), not a blanket
"parse every string", so well-behaved providers are untouched.

Related: the `feedback`/memory note on the qwen double-encoding quirk.
