// `frances:v1/agent-sections` — instruction discovery prompt sections.
//
// Three stable section objects for `session.promptSections`:
//
//   - `globalAgents` — emits discovered global instructions, labelled by source.
//   - `localAgents` — emits discovered local instructions, labelled by source.
//   - `nestedAgentsInventory` — emits nested AGENTS.md path list with read-nudge.
//
// Each returns null when no files are found. All three are async because
// they call Rust-backed discovery functions that perform filesystem I/O.

import {
  discoverGlobalAgents,
  discoverLocalAgents,
  discoverNestedAgents,
} from "frances:v1/agents";

/**
 * Global instruction files section.
 * Emits content from ~/.claude/CLAUDE.md, XDG dirs, and $HOME/AGENTS.md,
 * lowest-priority first, labelled by path.
 */
export const globalAgents = {
  name: "global-agents",

  async prompt(ctx) {
    const files = await discoverGlobalAgents();
    if (files == null) return null;

    const parts = [];
    parts.push(
      "Global instruction files (lowest priority first; later entries take precedence):",
    );
    for (const file of files) {
      parts.push("");
      parts.push(`--- ${file.path} ---`);
      parts.push(file.content);
    }
    parts.push("");
    parts.push(
      "Project and local instructions take precedence over global instructions.",
    );
    return parts.join("\n");
  },
};

/**
 * Project/local instruction files section.
 * Emits content from the first editable root (CLAUDE.md, AGENTS.md, etc.),
 * lowest-priority first, labelled by path.
 */
export const localAgents = {
  name: "local-agents",

  async prompt(ctx) {
    const files = await discoverLocalAgents();
    if (files == null) return null;

    const parts = [];
    parts.push(
      "Project instruction files (lowest priority first; later entries take precedence):",
    );
    for (const file of files) {
      parts.push("");
      parts.push(`--- ${file.path} ---`);
      parts.push(file.content);
    }
    parts.push("");
    parts.push(
      "`.local` files take precedence over shared files. Project instructions take precedence over global instructions.",
    );
    return parts.join("\n");
  },
};

/**
 * Nested instruction files inventory section.
 * Lists paths of AGENTS.md files in subdirectories (excludes root-level files
 * covered by localAgents). Includes a nudge to read before working in those subtrees.
 */
export const nestedAgentsInventory = {
  name: "nested-agents-inventory",

  async prompt(ctx) {
    const paths = await discoverNestedAgents();
    if (paths == null) return null;

    const parts = [];
    parts.push("Nested instruction files found in subdirectories:");
    for (const path of paths) {
      parts.push(`- ${path}`);
    }
    parts.push("");
    parts.push(
      "Before working in a subtree that contains one of these files, read it with `file_read` to understand subtree-specific standards.",
    );
    return parts.join("\n");
  },
};

