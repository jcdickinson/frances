// `frances:v1/tools/file_search` — combined name-pattern lookup, content
// search, and directory listing tool.
//
// `FileSearch` is the Rust-backed primitive: one async `search(args)`
// method that returns a JSON-string payload. `Search` is the LLM-facing
// tool class shaped for `chat.tools.push(...)`. It takes a `Variables`
// instance so the `into` arg can route the full structured result into
// a Frances variable, while the LLM still gets a compact inline preview.
//
// Typical wiring:
//
//   const fs   = new FileSearch();
//   const vars = new Variables();
//   chat.tools.push(new Search(fs, vars));

const { FileSearch, FileSearchDescriptions: desc } =
  globalThis.__frances_v1_stash__;

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
      description: "Max walk depth. Omit for unbounded.",
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

// Render the full result as a compact, scannable text block. One line per
// entry. With `search`, lines look like `path:line:text  (N matches)`;
// without, `path  (size B)`. Truncation banner appended if present.
function _formatInline(result, hasSearch) {
  const lines = [];
  if (result.entries.length === 0) {
    lines.push(hasSearch ? "no matches" : "no files");
  } else {
    for (const e of result.entries) {
      if (hasSearch && e.first_match) {
        lines.push(
          `${e.path}:${e.first_match.line}:${e.first_match.text}  (${e.match_count} matches)`,
        );
      } else if (hasSearch) {
        lines.push(`${e.path}  (${e.match_count} matches)`);
      } else {
        lines.push(`${e.path}  (${e.size}B)`);
      }
    }
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
  const preview = _formatInline({ entries: head, truncated: null }, hasSearch);
  const headLine = `${varName} = ${n}${result.truncated ? "+" : ""} entries`;
  const truncLine = result.truncated ? `\n${result.truncated.message}` : "";
  return `${headLine}\n${preview}${truncLine}`;
}

class Search {
  static schema = SEARCH_SCHEMA;

  constructor(fileSearch, vars) {
    this.fileSearch = fileSearch;
    this.vars = vars;
    this.name = "file_search";
    this.description = desc.file_search;
    this.parameters = SEARCH_SCHEMA;
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
