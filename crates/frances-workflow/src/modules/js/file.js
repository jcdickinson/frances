// `frances:v1/tools/file` — anchor-aware Editor primitive + per-op
// tool classes for `chat.tools.push(...)`.
//
// `Editor` is a Rust-backed handle on the daemon's session-scoped
// `EditSession`. Each `new Editor()` returns a clone of the same
// underlying session so the anchor cache is shared across a workflow.
// The tool classes are thin JS wrappers around Editor's three methods
// (`readFile`, `readRaw`, `edit`).
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
//     new Replace(editor, vars),
//     new InsertAfter(editor, vars),
//     new InsertBefore(editor, vars),
//     new New(editor, vars),
//     new Overwrite(editor, vars),
//   );

const { Editor, EditorDescriptions: desc } = globalThis.__frances_v1_stash__;

// ---- schemas --------------------------------------------------------------

const READ_SCHEMA = {
  type: "object",
  properties: {
    path: { type: "string" },
    into: {
      type: "string",
      description:
        "Optional Frances variable name to store the file's raw bytes into. " +
        "Bypasses anchors and does NOT count as a read for editing — call " +
        "file_read without `into` if you intend to edit the file afterwards.",
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

  handler = async ({ call }) => {
    const { path, into } = call.arguments;
    try {
      if (into) {
        const raw = await this.editor.readRaw(path);
        this.vars.set(into, raw);
        return _okResult(call.id, `${into} = string`);
      }
      const content = await this.editor.readFile(path);
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

class Replace {
  static schema = REPLACE_SCHEMA;

  constructor(editor, vars) {
    this.editor = editor;
    this.vars = vars;
    this.name = "file_replace";
    this.description = desc.file_replace;
    this.parameters = REPLACE_SCHEMA;
  }

  handler = async ({ call }) => {
    let text;
    try {
      text = _resolveText(call.arguments, this.vars);
    } catch (err) {
      return _errResult(call.id, err);
    }
    try {
      const content = await this.editor.edit({
        kind: "Replace",
        path: call.arguments.path,
        anchor: call.arguments.anchor,
        end_anchor: call.arguments.end_anchor,
        text,
      });
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

  handler = async ({ call }) => {
    let text;
    try {
      text = _resolveText(call.arguments, this.vars);
    } catch (err) {
      return _errResult(call.id, err);
    }
    try {
      const content = await this.editor.edit({
        kind: "InsertAfter",
        path: call.arguments.path,
        anchor: call.arguments.anchor,
        text,
      });
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

  handler = async ({ call }) => {
    let text;
    try {
      text = _resolveText(call.arguments, this.vars);
    } catch (err) {
      return _errResult(call.id, err);
    }
    try {
      const content = await this.editor.edit({
        kind: "InsertBefore",
        path: call.arguments.path,
        anchor: call.arguments.anchor,
        text,
      });
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

  handler = async ({ call }) => {
    let text;
    try {
      text = _resolveText(call.arguments, this.vars);
    } catch (err) {
      return _errResult(call.id, err);
    }
    try {
      const content = await this.editor.edit({
        kind: "New",
        path: call.arguments.path,
        text,
      });
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

  handler = async ({ call }) => {
    let text;
    try {
      text = _resolveText(call.arguments, this.vars);
    } catch (err) {
      return _errResult(call.id, err);
    }
    try {
      const content = await this.editor.edit({
        kind: "Overwrite",
        path: call.arguments.path,
        text,
      });
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

export { Editor, Read, Replace, InsertAfter, InsertBefore, New, Overwrite };
