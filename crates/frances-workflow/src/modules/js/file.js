// `frances:v1/tools/file` — anchor-aware Editor primitive + per-op
// tool classes for `chat.tools.push(...)`.
//
// `Editor` is a Rust-backed handle on the daemon's session-scoped
// `EditSession`. Each `new Editor()` returns a clone of the same
// underlying session so the anchor cache is shared across a workflow.
// The tool classes are thin JS wrappers that shape Editor's two
// methods (`readFile`, `edit`) into the LLM-facing `file_*` tool
// surface.
//
// Typical wiring:
//
//   const editor = new Editor();
//   chat.tools.push(
//     new Read(editor),
//     new Replace(editor),
//     new InsertAfter(editor),
//     new InsertBefore(editor),
//     new New(editor),
//     new Overwrite(editor),
//   );

const { Editor } = globalThis.__frances_v1_stash__;

// ---- schemas --------------------------------------------------------------

const READ_SCHEMA = {
  type: "object",
  properties: { path: { type: "string" } },
  required: ["path"],
};

const REPLACE_SCHEMA = {
  type: "object",
  properties: {
    path: { type: "string" },
    anchor: { type: "string" },
    end_anchor: { type: "string" },
    text: { type: "string" },
  },
  required: ["path", "anchor", "end_anchor", "text"],
};

const INSERT_SCHEMA = {
  type: "object",
  properties: {
    path: { type: "string" },
    anchor: { type: "string" },
    text: { type: "string" },
  },
  required: ["path", "anchor", "text"],
};

const WHOLE_FILE_SCHEMA = {
  type: "object",
  properties: {
    path: { type: "string" },
    text: { type: "string" },
  },
  required: ["path", "text"],
};

// ---- descriptions ---------------------------------------------------------

const READ_DESC =
  "Read a file from disk and render it with line anchors. Each line is rendered as `Word§content` — a stable per-line anchor word (e.g. `Apple`, `BananaCarrot`), then `§`, then the line's content. Blank lines render as `Word§` with empty content. Anchors survive external edits and formatter runs. The rendered string of each line is exactly what you pass back as the `anchor` (and `end_anchor`) field of an `edit` call. Always call `file_read` for a path before calling `edit` on it — edit requires the file to be cached this turn. The path may be absolute or relative to the client's working directory.";

const REPLACE_DESC = `Replace one contiguous range of lines in a file with new content.

Args: \`{ path, anchor, end_anchor, text }\`

  path:        the file to edit. Must have been read this turn via \`file_read\` (or just created with \`file_new\` / \`file_overwrite\`, which echo back anchors).
  anchor:      full rendered anchor line of the FIRST line in the range — \`Word§content\`, exactly as \`file_read\` produced it.
  end_anchor:  full rendered anchor line of the LAST line in the range (inclusive). For a single-line replace, pass the same value as \`anchor\`.
  text:        the replacement content. Use \`\\n\` for newlines. Multi-line is fine; do NOT include any anchors in \`text\`.

Anchor protocol: every line in a \`file_read\` (or post-edit diff) is rendered as \`Word§content\`. The \`Word\` half identifies the line by anchor; the content half is what's currently on that line. Pass back both halves verbatim — the engine splits on the first \`§\`, validates the anchor word against the cached file, and compares the content (trimmed) for safety. On a content mismatch the call fails and you should re-read the file before retrying.

After every edit the file is run through the project formatter and written to disk; the returned diff block reflects the post-format content.

WORKED EXAMPLE. After \`file_read\` on \`src/greet.py\` returns:

  Apple§def hello():
  Banana§    print("hi")
  Cherry§
  Daisy§def goodbye():

To replace the print with two prints:

{
  "path": "src/greet.py",
  "anchor":     "Banana§    print(\\"hi\\")",
  "end_anchor": "Banana§    print(\\"hi\\")",
  "text":       "    print(\\"hi there\\")\\n    print(\\"welcome\\")"
}

Returns the diff block for the file with new anchors for inserted lines.`;

const INSERT_AFTER_DESC = `Insert new content immediately after a specific line in a file.

Args: \`{ path, anchor, text }\`

  path:    the file to edit. Must have been read this turn via \`file_read\` (or just created with \`file_new\` / \`file_overwrite\`, which echo back anchors).
  anchor:  full rendered anchor line that the new content will be inserted AFTER — \`Word§content\`, exactly as \`file_read\` produced it.
  text:    the content to insert. Use \`\\n\` for newlines. Multi-line is fine; do NOT include any anchors in \`text\`.

Anchor protocol: every line in a \`file_read\` (or post-edit diff) is rendered as \`Word§content\`. The \`Word\` half identifies the line by anchor; the content half is what's currently on that line. Pass back both halves verbatim — the engine splits on the first \`§\`, validates the anchor word against the cached file, and compares the content (trimmed) for safety. On a content mismatch the call fails and you should re-read the file before retrying.

After every edit the file is run through the project formatter and written to disk; the returned diff block reflects the post-format content.

WORKED EXAMPLE. After \`file_read\` on \`src/greet.py\` returns:

  Apple§def hello():
  Banana§    print("hi")
  Daisy§def goodbye():

To add a comment after the print:

{
  "path": "src/greet.py",
  "anchor": "Banana§    print(\\"hi\\")",
  "text":   "    # printed greeting"
}

Returns the diff block for the file with new anchors for inserted lines.`;

const INSERT_BEFORE_DESC = `Insert new content immediately before a specific line in a file.

Args: \`{ path, anchor, text }\`

  path:    the file to edit. Must have been read this turn via \`file_read\` (or just created with \`file_new\` / \`file_overwrite\`, which echo back anchors).
  anchor:  full rendered anchor line that the new content will be inserted BEFORE — \`Word§content\`, exactly as \`file_read\` produced it.
  text:    the content to insert. Use \`\\n\` for newlines. Multi-line is fine; do NOT include any anchors in \`text\`.

Anchor protocol: every line in a \`file_read\` (or post-edit diff) is rendered as \`Word§content\`. The \`Word\` half identifies the line by anchor; the content half is what's currently on that line. Pass back both halves verbatim — the engine splits on the first \`§\`, validates the anchor word against the cached file, and compares the content (trimmed) for safety. On a content mismatch the call fails and you should re-read the file before retrying.

After every edit the file is run through the project formatter and written to disk; the returned diff block reflects the post-format content.

WORKED EXAMPLE. After \`file_read\` on \`src/greet.py\` returns:

  Apple§def hello():
  Banana§    print("hi")
  Daisy§def goodbye():

To add a comment before \`goodbye\`:

{
  "path": "src/greet.py",
  "anchor": "Daisy§def goodbye():",
  "text":   "# Says goodbye."
}

Returns the diff block for the file with new anchors for inserted lines.`;

const NEW_DESC = `Create a new file with the given content.

Args: \`{ path, text }\`

  path:  the file to create. Must NOT already exist on disk — for an existing file use \`file_overwrite\` instead (which requires a prior \`file_read\` so you've seen the content you're replacing). Missing parent directories are created automatically.
  text:  the file's content. Use \`\\n\` for newlines.

You do NOT need to call \`file_read\` first — the file doesn't exist yet. The response echoes the file back with fresh anchors on every line, so subsequent \`file_replace\` / \`file_insert_after\` / \`file_insert_before\` calls against the same path can use those anchors directly without a separate read.

After every edit the file is run through the project formatter and written to disk; the returned content reflects the post-format result.

WORKED EXAMPLE.

{
  "path": "src/notes.md",
  "text": "# Notes\\n\\nfirst draft\\n"
}

Returns the new file rendered as \`Word§content\` lines (one per source line).`;

const OVERWRITE_DESC = `Replace the entire content of an existing file.

Args: \`{ path, text }\`

  path:  the file to overwrite. You MUST have called \`file_read\` on this path this turn — overwrite is destructive, so the engine requires you to have seen the prior content first.
  text:  the new content. Use \`\\n\` for newlines.

Use this when you'd otherwise need many overlapping line edits, or when you're rewriting a file from scratch. For creating a brand-new file, use \`file_new\` instead. For surgical changes, prefer \`file_replace\` / \`file_insert_after\` / \`file_insert_before\`.

The response echoes the post-write file back with fresh anchors on every line (the prior anchors are tombstoned), so subsequent \`file_replace\` / \`file_insert_*\` calls against the same path can use the new anchors directly without re-reading.

After every edit the file is run through the project formatter and written to disk; the returned diff block reflects the post-format content.

WORKED EXAMPLE.

{
  "path": "src/config.toml",
  "text": "[server]\\nport = 8080\\n"
}

Returns the diff block showing every prior line removed and the new lines minted with fresh anchors.`;

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

// ---- tool classes ---------------------------------------------------------

class Read {
  static schema = READ_SCHEMA;

  constructor(editor) {
    this.editor = editor;
    this.name = "file_read";
    this.description = READ_DESC;
    this.parameters = READ_SCHEMA;
  }

  handler = async ({ call }) => {
    try {
      const content = await this.editor.readFile(call.arguments.path);
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

class Replace {
  static schema = REPLACE_SCHEMA;

  constructor(editor) {
    this.editor = editor;
    this.name = "file_replace";
    this.description = REPLACE_DESC;
    this.parameters = REPLACE_SCHEMA;
  }

  handler = async ({ call }) => {
    try {
      const content = await this.editor.edit({
        kind: "Replace",
        path: call.arguments.path,
        anchor: call.arguments.anchor,
        end_anchor: call.arguments.end_anchor,
        text: call.arguments.text,
      });
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

class InsertAfter {
  static schema = INSERT_SCHEMA;

  constructor(editor) {
    this.editor = editor;
    this.name = "file_insert_after";
    this.description = INSERT_AFTER_DESC;
    this.parameters = INSERT_SCHEMA;
  }

  handler = async ({ call }) => {
    try {
      const content = await this.editor.edit({
        kind: "InsertAfter",
        path: call.arguments.path,
        anchor: call.arguments.anchor,
        text: call.arguments.text,
      });
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

class InsertBefore {
  static schema = INSERT_SCHEMA;

  constructor(editor) {
    this.editor = editor;
    this.name = "file_insert_before";
    this.description = INSERT_BEFORE_DESC;
    this.parameters = INSERT_SCHEMA;
  }

  handler = async ({ call }) => {
    try {
      const content = await this.editor.edit({
        kind: "InsertBefore",
        path: call.arguments.path,
        anchor: call.arguments.anchor,
        text: call.arguments.text,
      });
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

class New {
  static schema = WHOLE_FILE_SCHEMA;

  constructor(editor) {
    this.editor = editor;
    this.name = "file_new";
    this.description = NEW_DESC;
    this.parameters = WHOLE_FILE_SCHEMA;
  }

  handler = async ({ call }) => {
    try {
      const content = await this.editor.edit({
        kind: "New",
        path: call.arguments.path,
        text: call.arguments.text,
      });
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

class Overwrite {
  static schema = WHOLE_FILE_SCHEMA;

  constructor(editor) {
    this.editor = editor;
    this.name = "file_overwrite";
    this.description = OVERWRITE_DESC;
    this.parameters = WHOLE_FILE_SCHEMA;
  }

  handler = async ({ call }) => {
    try {
      const content = await this.editor.edit({
        kind: "Overwrite",
        path: call.arguments.path,
        text: call.arguments.text,
      });
      return _okResult(call.id, content);
    } catch (err) {
      return _errResult(call.id, err);
    }
  };
}

export { Editor, Read, Replace, InsertAfter, InsertBefore, New, Overwrite };
