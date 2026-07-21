// `frances:v1/tools/file_find_or_grep` — combined name-pattern lookup,
// content search, and directory listing tool.
//
// `FileSearch` is the Rust-backed primitive: one async `search(args)`
// method that returns a JSON-string payload. It binds to an `Editor` so it
// shares that context's loop guard (an edit clears it, and it resets when the
// context clears). `Search` is the LLM-facing tool class shaped for
// `chat.tools.push(...)`. It takes a `Variables` instance so the `into` arg
// can route the full structured result into a Frances variable, while the LLM
// still gets a compact inline preview.
//
// Typical wiring:
//
//   const editor = new Editor();
//   const fs     = new FileSearch(editor);
//   const vars   = new Variables();
//   chat.tools.push(new Search(fs, vars));

import fileFindOrGrepDescription from "./file_find_or_grep.md";

const { FileSearch } = globalThis.__frances_v1_stash__;

// Keep one grep call to a few thousand tokens even when it finds hundreds of
// files. Rust separately bounds each matching-line preview; this bounds their
// aggregate after formatting for the model.
const INLINE_RESULT_BYTE_CAP = 16 * 1024;
const INLINE_NOTICE_RESERVE = 512;

const SEARCH_SCHEMA = {
  type: "object",
  properties: {
    paths: {
      type: "array",
      items: { type: "string" },
      description:
        "Include globs (ripgrep dialect: `**`, `{a,b}`). Omit for everything under cwd.",
    },
    search: {
      type: "string",
      description:
        "Regex (Rust dialect). When set, results are filtered to files containing at least one match.",
    },
    exclude: {
      type: "array",
      items: { type: "string" },
      description: "Globs to subtract from `paths`.",
    },
    ignore: {
      type: "boolean",
      description:
        "Honor `.gitignore`/`.ignore`/`.rgignore`. Default `true`.",
    },
    hidden: {
      type: "boolean",
      description: "Include hidden files and directories. Default `false`.",
    },
    depth: {
      type: "integer",
      minimum: 0,
      description:
        "Max walk depth. Omit for unbounded. Depth counts from each starting point: 0 includes only the starting path itself, 1 includes immediate children, 2 includes grandchildren, etc. Use 1 to list files directly in cwd.",
    },
    paths_only: {
      type: "boolean",
      description:
        "With `search`, suppress `first_match` and keep only `match_count`.",
    },
    into: {
      type: "string",
      description:
        "Frances variable name. When set, the full result lands in the variable AND an inline summary is returned.",
    },
  },
};

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

function _formatEntry(e, hasSearch) {
  if (hasSearch && e.first_match) {
    const truncation = e.first_match.text_truncated
      ? `  [line truncated from ${e.first_match.line_bytes}B]`
      : "";
    const location = `${e.path}:${e.first_match.line}:${e.first_match.text}`;
    return `${location}${truncation}  (${e.match_count} matches)`;
  }
  if (hasSearch) return `${e.path}  (${e.match_count} matches)`;
  return `${e.path}  (${e.size}B)`;
}

function _utf8Length(text) {
  let bytes = 0;
  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i);
    if (code < 0x80) {
      bytes += 1;
    } else if (code < 0x800) {
      bytes += 2;
    } else if (
      code >= 0xd800 &&
      code <= 0xdbff &&
      i + 1 < text.length &&
      text.charCodeAt(i + 1) >= 0xdc00 &&
      text.charCodeAt(i + 1) <= 0xdfff
    ) {
      bytes += 4;
      i += 1;
    } else {
      bytes += 3;
    }
  }
  return bytes;
}

// Render a bounded, scannable text block. One line per entry. The notice
// reserve guarantees that omission is always explicit rather than silently
// consuming the entire budget with entry text.
function _formatInline(result, hasSearch, byteCap = INLINE_RESULT_BYTE_CAP) {
  const lines = [];
  const entryCap = Math.max(0, byteCap - INLINE_NOTICE_RESERVE);
  let bytes = 0;
  let shown = 0;
  if (result.entries.length === 0) {
    lines.push(hasSearch ? "no matches" : "no files");
  } else {
    for (const e of result.entries) {
      const line = _formatEntry(e, hasSearch);
      const addedBytes = _utf8Length(line) + (lines.length === 0 ? 0 : 1);
      if (bytes + addedBytes > entryCap) break;
      lines.push(line);
      bytes += addedBytes;
      shown += 1;
    }
  }
  const omitted = result.entries.length - shown;
  if (omitted > 0) {
    lines.push("");
    lines.push(
      `… ${omitted} entries omitted from this response (${INLINE_RESULT_BYTE_CAP / 1024} KiB output limit); narrow paths/search or use into`,
    );
  }
  if (result.truncated) {
    lines.push("");
    lines.push(result.truncated.message);
  }
  return lines.join("\n");
}

// Inline summary when `into` is set: count, plus a peek at the first 5
// entries shaped like the inline format. The full structured result is
// already in the variable, so this is purely for context.
function _formatSummary(varName, result, hasSearch) {
  const n = result.entries.length;
  const head = result.entries.slice(0, 5);
  const headLine = `${varName} = ${n}${result.truncated ? "+" : ""} entries`;
  const truncLine = result.truncated ? `\n${result.truncated.message}` : "";
  const previewCap =
    INLINE_RESULT_BYTE_CAP - _utf8Length(headLine) - _utf8Length(truncLine) - 2;
  const preview = _formatInline(
    { entries: head, truncated: null },
    hasSearch,
    previewCap,
  );
  return `${headLine}\n${preview}${truncLine}`;
}

class Search {
  static schema = SEARCH_SCHEMA;

  constructor(fileSearch, vars) {
    this.fileSearch = fileSearch;
    this.vars = vars;
    this.name = "file_find_or_grep";
    this.description = fileFindOrGrepDescription;
    this.parameters = SEARCH_SCHEMA;
  }

  describe(call) {
    const a = call.arguments || {};
    const parts = [];
    if (typeof a.search === "string" && a.search.length > 0) {
      parts.push(`/${a.search}/`);
    }
    if (Array.isArray(a.paths) && a.paths.length > 0) {
      parts.push(a.paths.join(" "));
    }
    return parts.join(" in ");
  }

  handler = async ({ call }) => {
    const args = { ...call.arguments };
    const into = args.into;
    delete args.into;
    let payload;
    try {
      const json = await this.fileSearch.search(args);
      payload = JSON.parse(json);
    } catch (err) {
      return _errResult(call.id, err);
    }
    const hasSearch = typeof args.search === "string" && args.search.length > 0;
    if (typeof into === "string" && into.length > 0) {
      this.vars.set(into, payload);
      return _okResult(call.id, _formatSummary(into, payload, hasSearch));
    }
    return _okResult(call.id, _formatInline(payload, hasSearch));
  };
}

export { FileSearch, Search };
