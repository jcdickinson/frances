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
// Tool descriptions and family guidance live as `.md` files next to
// this module and import as default strings through the embedded VFS.
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

import { transcript, DiffSection, EntityRefSection } from "frances:v1/sections";
import { createEntity } from "frances:v1/entities";
import { defineToolFamily } from "frances:v1/tool-family";
import editingFamilyPrompt from "./editing_family.md";
import fileReadDescription from "./file_read.md";
import fileReplaceLinesDescription from "./file_replace_lines.md";
import fileReplaceAllDescription from "./file_replace_all.md";
import fileInsertAfterDescription from "./file_insert_after.md";
import fileInsertBeforeDescription from "./file_insert_before.md";
import fileNewDescription from "./file_new.md";
import fileOverwriteDescription from "./file_overwrite.md";

const { Editor } = globalThis.__frances_v1_stash__;

const editingFamily = defineToolFamily({
  prompt() {
    return editingFamilyPrompt;
  },
});

// Ship the structured diff portion of `editor.edit()`'s result to the
// UI as a one-shot `DiffSection`. The string portion is returned to the
// LLM as the tool's content; the structured ops only travel to the
// transcript. Skips empty payloads so no-op edits (replace_all with
// zero matches, overwrites that didn't change anything) don't paint a
// blank diff block.
function _pushDiffSection(diff) {
  if (Array.isArray(diff) && diff.length > 0) {
    transcript.push(new DiffSection({ lines: diff }));
  }
}

// Split a `file_read` render into blocks of parsed lines. Each rendered
// line is `prefix<sep>content`: an anchor word before `§` in-repo, a
// 1-indexed line number before `:` out-of-repo (anchor words are never
// all digits, so the two never get confused). A lone `…§` / `…` line
// separates the blocks of two non-adjacent ranges.
function _readBlocks(rendered) {
  const blocks = [[]];
  for (const raw of rendered.split("\n")) {
    if (raw === "…§" || raw === "…") {
      blocks.push([]);
      continue;
    }
    const colon = raw.indexOf(":");
    if (colon > 0 && /^\d+$/.test(raw.slice(0, colon))) {
      blocks[blocks.length - 1].push({
        line: Number(raw.slice(0, colon)),
        anchor: null,
        text: raw.slice(colon + 1),
      });
      continue;
    }
    const sep = raw.indexOf("§");
    if (sep < 0) continue; // trailing "" from the final newline
    blocks[blocks.length - 1].push({
      line: null,
      anchor: raw.slice(0, sep),
      text: raw.slice(sep + 1),
    });
  }
  return blocks;
}

// The requested ranges as Rust rendered them: sorted, with adjacent and
// overlapping ones joined. Rust also clamps to EOF, which needs the
// file's length — see `_numberBlocks` for how that's absorbed. No ranges
// means the whole file, i.e. one block starting at line 1.
function _mergeRanges(ranges) {
  if (!ranges || ranges.length === 0) return [[1, Infinity]];
  const merged = [];
  for (const [start, end] of [...ranges].sort((a, b) => a[0] - b[0])) {
    const last = merged[merged.length - 1];
    if (last && start <= last[1] + 1) {
      last[1] = Math.max(last[1], end);
    } else {
      merged.push([start, end]);
    }
  }
  return merged;
}

// Fill in the line numbers an anchored render doesn't carry, taking each
// block's first line from the matching requested range. Clamping to EOF
// can leave the final block short and drop ranges past the end, so those
// are tolerated; any other disagreement means the merge above didn't
// reproduce what Rust rendered, and the numbers are left off rather than
// guessed at.
function _numberBlocks(blocks, ranges) {
  const merged = _mergeRanges(ranges);
  if (blocks.length > merged.length) return;
  for (const [i, lines] of blocks.entries()) {
    const [start, end] = merged[i];
    const requested = end - start + 1;
    const short = lines.length < requested;
    if (lines.length > requested || (short && i < blocks.length - 1)) return;
  }
  for (const [i, lines] of blocks.entries()) {
    lines.forEach((row, offset) => (row.line = merged[i][0] + offset));
  }
}

// Publish a read as a `file` entity so the UI can render the code the
// model just looked at. A read is complete the moment it returns, so the
// entity is born settled — nothing ever streams into it.
function _pushReadEntity(path, rendered, ranges) {
  const blocks = _readBlocks(rendered);
  const anchored = blocks.flat().some((row) => row.line === null);
  if (anchored) _numberBlocks(blocks, ranges);
  const rows = [];
  for (const lines of blocks) {
    if (rows.length > 0) rows.push({ kind: "gap" });
    for (const row of lines) rows.push({ kind: "line", ...row });
  }
  const snapshot = { path, rows };
  const handle = createEntity("file", snapshot);
  handle.settle(snapshot);
  transcript.push(new EntityRefSection({ id: handle.id }));
}

// ---- schemas --------------------------------------------------------------

const READ_SCHEMA = {
  type: "object",
  properties: {
    path: { type: "string" },
    ranges: {
      type: "array",
      description:
        "Optional list of 1-indexed, inclusive [start, end] pairs. Returned output concatenates the requested ranges with separator `…§`. Muxually exclusive with `into`.",
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
        "Bypasses anchors and does NOT count as a read for editing — I MUST call " +
        "file_read without `into` if I intend to edit the file afterwards. Mutually exclusive with `ranges`.",
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
    throw new Error("I provide exactly one of `text` or `from`, not both");
  }
  if (!hasText && !hasFrom) {
    throw new Error("I provide exactly one of `text` or `from`");
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

// ---- tool classes ---------------------------------------------------------

class Read {
  static schema = READ_SCHEMA;

  constructor(editor, vars) {
    this.editor = editor;
    this.vars = vars;
    this.name = "file_read";
    this.description = fileReadDescription;
    this.parameters = READ_SCHEMA;
  }

  handler = async ({ call }) => {
    const { path, into, ranges } = call.arguments;
    try {
      if (into && ranges) {
        return _errResult(
          call.id,
          new Error("I provide exactly one of `into` or `ranges`, not both"),
        );
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
      _pushReadEntity(path, content, ranges);
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
    this.description = fileReplaceLinesDescription;
    this.parameters = REPLACE_SCHEMA;
    this.family = editingFamily;
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
      _pushDiffSection(diff);
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
    this.description = fileReplaceAllDescription;
    this.parameters = REPLACE_ALL_SCHEMA;
    this.family = editingFamily;
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
      _pushDiffSection(diff);
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
    this.description = fileInsertAfterDescription;
    this.parameters = INSERT_SCHEMA;
    this.family = editingFamily;
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
      _pushDiffSection(diff);
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
    this.description = fileInsertBeforeDescription;
    this.parameters = INSERT_SCHEMA;
    this.family = editingFamily;
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
      _pushDiffSection(diff);
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
    this.description = fileNewDescription;
    this.parameters = WHOLE_FILE_SCHEMA;
    this.family = editingFamily;
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
      _pushDiffSection(diff);
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
    this.description = fileOverwriteDescription;
    this.parameters = WHOLE_FILE_SCHEMA;
    this.family = editingFamily;
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
      _pushDiffSection(diff);
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
  editingFamily,
};
