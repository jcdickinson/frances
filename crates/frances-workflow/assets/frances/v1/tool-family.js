// `frances:v1/tool-family` — `defineToolFamily`, `defineTool`, and `toolGuidance`.
//
// A **ToolFamily** is a shared preamble object referenced by identity (`===`).
// Tools point at their family; membership is one-way (tool → family).
// The family never lists its members — "which families are present" is derived
// from "which tools are available" at assembly time.
//
// A **tool object** carries the LLM-facing schema (name, description, parameters),
// an optional family reference, and a handler. It is pushed onto
// `chat.tools` and serialized from there.
//
// `toolGuidance` is a prompt section that folds families from `ctx.tools`,
// deduplicates by identity (`===`), calls each present family's `prompt(ctx)`
// once, and joins the results. Returns null when no families are present.
//
// All helpers are pure JS with no Rust backing.

/**
 * Define a tool family — a shared preamble object for dedupe-by-identity.
 *
 * @param {{ prompt: (ctx: any) => string | null }} opts
 * @returns {{ prompt: (ctx: any) => string | null }} The identity object.
 */
function defineToolFamily({ prompt }) {
  if (typeof prompt !== "function") {
    throw new TypeError("defineToolFamily: prompt must be a function");
  }
  // Frozen so === identity is stable and callers can't mutate across tools.
  return Object.freeze({ prompt });
}

/**
 * Define a tool object with optional family membership.
 *
 * @param {{
 *   name: string,
 *   description: string,
 *   parameters: object,
 *   family?: { prompt: (ctx: any) => string | null },
 *   handler: (args: { call: any, scope: any }) => Promise<object>,
 * }} opts
 * @returns The tool object suitable for `chat.tools.push(...)`.
 */
function defineTool({ name, description, parameters, family, handler }) {
  if (typeof name !== "string" || name.length === 0) {
    throw new TypeError("defineTool: name must be a non-empty string");
  }
  if (typeof description !== "string") {
    throw new TypeError("defineTool: description must be a string");
  }
  if (typeof parameters !== "object" || parameters === null) {
    throw new TypeError("defineTool: parameters must be an object");
  }
  if (family !== undefined && family !== null) {
    if (typeof family !== "object" || family === null || typeof family.prompt !== "function") {
      throw new TypeError("defineTool: family must be a ToolFamily (object with a prompt function)");
    }
  }
  if (typeof handler !== "function") {
    throw new TypeError("defineTool: handler must be a function");
  }
  const tool = {
    name,
    description,
    parameters,
    handler,
  };
  // Attach family only when provided — absent means no family, which is valid.
  if (family !== undefined && family !== null) {
    tool.family = family;
  }
  return tool;
}

/**
 * `toolGuidance` — a prompt section that renders the union of all families
 * present in `ctx.tools`. Families are deduplicated by identity (`===`),
 * so each family's `prompt(ctx)` is called at most once regardless of how
 * many tools reference it. Returns null when no tools have families.
 *
 * Author-positioned in `promptSections` — ChatSession does not silently
 * append it.
 */
const toolGuidance = {
  name: "tool-guidance",

  prompt(ctx) {
    const seen = new Set();
    const parts = [];
    for (const tool of ctx.tools) {
      const fam = tool.family;
      if (fam && !seen.has(fam)) {
        seen.add(fam);
        const text = fam.prompt(ctx);
        if (text != null) parts.push(text);
      }
    }
    return parts.length > 0 ? parts.join("\n\n") : null;
  },
};

export { defineToolFamily, defineTool, toolGuidance };

