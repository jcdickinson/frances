// `frances:v1/tools/file` — anchor-aware Editor primitive + per-op
// tool classes for `chat.tools.push(...)`.
//
// `Editor` is a Rust-backed handle on the runtime's session-scoped
// `EditSession`. Each `new Editor()` returns a clone of the same
// underlying session so the anchor cache is shared across a workflow.
// The tool classes are thin JS wrappers around Editor's read/edit
// methods (`readFile`, `readRaw`, `edit`). `editor.commit()` reconciles
// accumulated edits (clears anchor tombstones) — the workflow calls it
// at its own turn boundary; the host no longer fires it automatically.
//
// Variables integration: every tool class takes a `Variables` instance
// alongside the editor. `Read` accepts optional `into: "<varname>"`
// which routes the file's raw bytes into the var (no anchors, no
// EditSession registration). The write classes accept optional
// `from: "<varname>"` in place of `text`, pulling the new content from
// a stored value (string verbatim, non-string `JSON.stringify`-encoded).
//
// Tool descriptions live as `.md` files next to this module and are
// pulled in via `include_str!` on the Rust side, then handed to us
// through the stash.
//
// Typical wiring:
//
//   const editor = new Editor();
//   const vars   = new Variables();
//   chat.tools.push(
//     new Read(editor, vars),
//     new ReplaceLines(editor, vars),
//     new ReplaceAll(editor, vars),
//     new InsertAfter(editor, vars),
//     new InsertBefore(editor, vars),
//     new New(editor, vars),
//     new Overwrite(editor, vars),
//   );

import { transcript, DiffFrame } from "frances:v1/frames";

const { Editor, EditorDescriptions: desc } = globalThis.__frances_v1_stash__;

// Ship the structured diff portion of `editor.edit()`'s result to the
// TUI as a one-shot `DiffFrame`. The string portion is returned to the
// LLM as the tool's content; the structured ops only travel to the
// transcript. Skips empty payloads so no-op edits (replace_all with
// zero matches, overwrites that didn't change anything) don't paint a
// blank diff block.
function _pushDiffFrame(diff) {
  if (Array.isArray(diff) && diff.length > 0) {
    transcript.push(new DiffFrame({ lines: diff }));
  }
}

// ---- schemas --------------------------------------------------------------

const READ_SCHEMA = {
  type: "object",
  properties: {
    path: { type: "string" },
    ranges: {
      type: "array",
      description: "Optional list of 1-indexed, inclusive [start, end] pairs. Returned output concatenates the requested ranges with separator `…§`. Muxually exclusive with `into`.",
      items: {
        type: "array",
        items: { type: "integer" },
        minItems: 2,
        maxItems: 2,
      },
    },
    into: {
      type: "string",
      description:
        "Optional Frances variable name to store the file's raw bytes into. " +
        "Bypasses anchors and does NOT count as a read for editing — call " +
        "file_read without `into` if you intend to edit the file afterwards. Mutually exclusive with `ranges`.",
    },
  },
  required: ["path"],
};

const REPLACE_SCHEMA = {
  type: "object",
  properties: {
    path: { type: "string" },
    anchor: { type: "string" },
    end_anchor: { type: "string" },
    text: { type: "string" },
    from: {
      type: "string",
      description:
        "Frances variable name to pull replacement text from. Provide exactly one of `text` or `from`.",
    },
  },
  required: ["path", "anchor", "end_anchor"],
};

const REPLACE_ALL_SCHEMA = {
  type: "object",
  properties: {
    path: { type: "string" },
    find: { type: "string" },
    replacement: { type: "string" },
    count: {
      type: "integer",
      minimum: 0,
      description:
        "Optional maximum match count. If the regex matches more than this, the edit fails without writing.",
    },
  },
  required: ["path", "find", "replacement"],
};

const INSERT_SCHEMA = {
  type: "object",
  properties: {
    path: { type: "string" },
    anchor: { type: "string" },
    text: { type: "string" },
    from: {
      type: "string",
      description:
        "Frances variable name to pull insertion text from. Provide exactly one of `text` or `from`.",
    },
  },
  required: ["path", "anchor"],
};

const WHOLE_FILE_SCHEMA = {
  type: "object",
  properties: {
    path: { type: "string" },
    text: { type: "string" },
    from: {
      type: "string",
      description:
        "Frances variable name to pull the file content from. Provide exactly one of `text` or `from`.",
    },
  },
  required: ["path"],
};

// ---- helpers --------------------------------------------------------------

function _okResult(call_id, content) {
  return { role: "tool", call_id, content, is_error: false };
}

function _errResult(call_id, err) {
  return {
    role: "tool",
    call_id,
    content: String((err && err.message) || err),
    is_error: true,
  };
}

// Resolve the text payload for a write op from the call's `text` /
// `from` fields against the variable store. Throws on validation
// failure or an unknown `from` name. Stringification rule matches the
// rest of the shell/file from-injection surface: strings pass through
// raw, everything else is JSON.stringify-encoded.
function _resolveText(args, vars) {
  const hasText = args.text !== undefined;
  const hasFrom =
    args.from !== undefined && args.from !== null && args.from !== "";
  if (hasText && hasFrom) {
    throw new Error("provide exactly one of `text` or `from`, not both");
  }
  if (!hasText && !hasFrom) {
    throw new Error("provide exactly one of `text` or `from`");
  }
  if (hasFrom) {
    if (!vars.has(args.from)) {
      throw new Error(`unknown variable: ${args.from}`);
    }
    const v = vars.get(args.from);
    return typeof v === "string" ? v : JSON.stringify(v);
  }
  return args.text;
}

// Format a list of [start, end] line-range pairs as `[1-50, 100]`, with
// single-line ranges collapsed to one number. Returns `null` if the
// shape isn't an array of `[number, number]` so describe() can fall
// back gracefully on malformed input.
function _formatRanges(ranges) {
  if (!Array.isArray(ranges) || ranges.length === 0) return null;
  const parts = [];
  for (const r of ranges) {
    if (!Array.isArray(r) || r.length !== 2) return null;
    const [a, b] = r;
    if (typeof a !== "number" || typeof b !== "number") return null;
    parts.push(a === b ? String(a) : `${a}-${b}`);
  }
  return `[${parts.join(", ")}]`;
}

// ---- tool classes ---------------------------------------------------------

class Read {
  static schema = READ_SCHEMA;

  constructor(editor, vars) {
    this.editor = editor;
    this.vars = vars;
    this.name = "file_read";
    this.description = desc.file_read;
    this.parameters = READ_SCHEMA;
  }

  describe(call) {
    const { path, into, ranges } = call.arguments || {};
    if (!path) return "";
    if (into) return `${path} → ${into}`;
    const fmt = _formatRanges(ranges);
    return fmt ? `${path} ${fmt}` : path;
  }

  handler = async ({ call }) => {
    const { path, into, ranges } = call.arguments;
    try {
      if (into && ranges) {
        return _errResult(call.id, new Error("provide exactly one of `into` or `ranges`, not both"));
      }
      if (into) {
        const raw = await this.editor.readRaw(path);
        this.vars.set(into, raw);
        return _okResult(call.id, `${into} = string`);
      }
      const args = { path };
      if (ranges) {
        args.ranges = ranges;
      }
      const content = await this.editor.readFile(args);
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

class ReplaceLines {
  static schema = REPLACE_SCHEMA;

  constructor(editor, vars) {
    this.editor = editor;
    this.vars = vars;
    this.name = "file_replace_lines";
    this.description = desc.file_replace_lines;
    this.parameters = REPLACE_SCHEMA;
  }

  describe(call) {
    return (call.arguments && call.arguments.path) || "";
  }

  handler = async ({ call }) => {
    let text;
    try {
      text = _resolveText(call.arguments, this.vars);
    } catch (err) {
      return _errResult(call.id, err);
    }
    try {
      const { text: content, diff } = await this.editor.edit({
        kind: "ReplaceLines",
        path: call.arguments.path,
        anchor: call.arguments.anchor,
        end_anchor: call.arguments.end_anchor,
        text,
      });
      _pushDiffFrame(diff);
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}


class ReplaceAll {
  static schema = REPLACE_ALL_SCHEMA;

  constructor(editor, vars) {
    this.editor = editor;
    this.vars = vars;
    this.name = "file_replace_all";
    this.description = desc.file_replace_all;
    this.parameters = REPLACE_ALL_SCHEMA;
  }

  describe(call) {
    const a = call.arguments || {};
    if (!a.path) return "";
    return typeof a.count === "number" ? `${a.path} ×${a.count}` : a.path;
  }

  handler = async ({ call }) => {
    try {
      const { text: content, diff } = await this.editor.edit({
        kind: "ReplaceAll",
        path: call.arguments.path,
        find: call.arguments.find,
        replacement: call.arguments.replacement,
        count: call.arguments.count,
      });
      _pushDiffFrame(diff);
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

class InsertAfter {
  static schema = INSERT_SCHEMA;

  constructor(editor, vars) {
    this.editor = editor;
    this.vars = vars;
    this.name = "file_insert_after";
    this.description = desc.file_insert_after;
    this.parameters = INSERT_SCHEMA;
  }

  describe(call) {
    return (call.arguments && call.arguments.path) || "";
  }

  handler = async ({ call }) => {
    let text;
    try {
      text = _resolveText(call.arguments, this.vars);
    } catch (err) {
      return _errResult(call.id, err);
    }
    try {
      const { text: content, diff } = await this.editor.edit({
        kind: "InsertAfter",
        path: call.arguments.path,
        anchor: call.arguments.anchor,
        text,
      });
      _pushDiffFrame(diff);
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

class InsertBefore {
  static schema = INSERT_SCHEMA;

  constructor(editor, vars) {
    this.editor = editor;
    this.vars = vars;
    this.name = "file_insert_before";
    this.description = desc.file_insert_before;
    this.parameters = INSERT_SCHEMA;
  }

  describe(call) {
    return (call.arguments && call.arguments.path) || "";
  }

  handler = async ({ call }) => {
    let text;
    try {
      text = _resolveText(call.arguments, this.vars);
    } catch (err) {
      return _errResult(call.id, err);
    }
    try {
      const { text: content, diff } = await this.editor.edit({
        kind: "InsertBefore",
        path: call.arguments.path,
        anchor: call.arguments.anchor,
        text,
      });
      _pushDiffFrame(diff);
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

class New {
  static schema = WHOLE_FILE_SCHEMA;

  constructor(editor, vars) {
    this.editor = editor;
    this.vars = vars;
    this.name = "file_new";
    this.description = desc.file_new;
    this.parameters = WHOLE_FILE_SCHEMA;
  }

  describe(call) {
    return (call.arguments && call.arguments.path) || "";
  }

  handler = async ({ call }) => {
    let text;
    try {
      text = _resolveText(call.arguments, this.vars);
    } catch (err) {
      return _errResult(call.id, err);
    }
    try {
      const { text: content, diff } = await this.editor.edit({
        kind: "New",
        path: call.arguments.path,
        text,
      });
      _pushDiffFrame(diff);
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

class Overwrite {
  static schema = WHOLE_FILE_SCHEMA;

  constructor(editor, vars) {
    this.editor = editor;
    this.vars = vars;
    this.name = "file_overwrite";
    this.description = desc.file_overwrite;
    this.parameters = WHOLE_FILE_SCHEMA;
  }

  describe(call) {
    return (call.arguments && call.arguments.path) || "";
  }

  handler = async ({ call }) => {
    let text;
    try {
      text = _resolveText(call.arguments, this.vars);
    } catch (err) {
      return _errResult(call.id, err);
    }
    try {
      const { text: content, diff } = await this.editor.edit({
        kind: "Overwrite",
        path: call.arguments.path,
        text,
      });
      _pushDiffFrame(diff);
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

export {
  Editor,
  Read,
  ReplaceLines,
  ReplaceAll,
  InsertAfter,
  InsertBefore,
  New,
  Overwrite,
};
