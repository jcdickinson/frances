// `frances:v1/context-sections` — environment and cwd prompt sections.
//
// Two stable section objects for `session.promptSections`:
//
//   - `envBlock` (immutable, front of prompt): OS, shell, platform, repo root,
//     date, and the quasi-persistent shell guidance rule. Always emits a string.
//   - `cwdBlock` (mutable, late in prompt): the live working directory.
//     Returns null when cwd is unavailable.

import Mustache from "vendor:mustache";
import envTemplate from "./env.mustache.md";

/**
 * Immutable environment block — cache-stable across turns.
 * Contains OS, shell, platform, repo root, date, and shell behavior rules.
 */
export const envBlock = {
  name: "env",

  prompt(ctx) {
    return Mustache.render(envTemplate, ctx);
  },
};

/**
 * Live working directory — mutable, re-rendered each turn.
 * Placed late in the prompt to keep it out of the cache prefix.
 * Returns null when cwd is unavailable.
 */
export const cwdBlock = {
  name: "cwd",

  prompt(ctx) {
    if (ctx.cwd == null) return null;
    return `Current working directory: ${ctx.cwd}`;
  },
};
