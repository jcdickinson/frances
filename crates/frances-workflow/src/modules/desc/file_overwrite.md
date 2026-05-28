Replace the entire content of an existing file.

Args: `{ path, text }`

  path:  the file to overwrite. You MUST have called `file_read` on this path this turn — overwrite is destructive, so the engine requires you to have seen the prior content first.
  text:  the new content. Use `\n` for newlines.

CRITICAL: `text` must NEVER contain a `Word§` prefix unless you genuinely want those literal characters in the file (editing the anchor engine itself, a test fixture, prose with `Word§`). Anchors are read-only metadata the engine assigns — they're not part of line content. If you paste back the rendered prefixes from a `file_read`, they get written verbatim and your edit is broken.

  WRONG → text: "Apple§def hello():\nBanana§    print(\"hi\")"
  RIGHT → text: "def hello():\n    print(\"hi\")"

Provide exactly one of `text` or `from`:

  from:  a Frances variable name. Its value is used as the new content (string values pass through verbatim; non-string values are JSON-encoded). Use this when the content was prepared via `variable_set` / `variable_assign` / `file_read into:` / `shell_capture`, to avoid re-emitting a long payload in a tool-call.

Use this when you'd otherwise need many overlapping line edits, or when you're rewriting a file from scratch. For creating a brand-new file, use `file_new` instead. For surgical changes, prefer `file_replace_lines` / `file_insert_after` / `file_insert_before`.

The response echoes the post-write file back with fresh anchors on every line (the prior anchors are tombstoned), so subsequent `file_replace_lines` / `edit_insert_*` calls against the same path can use the new anchors directly without re-reading.

After every edit the file is run through the project formatter and written to disk; the returned diff block reflects the post-format content.

WORKED EXAMPLE.

{
  "path": "src/config.toml",
  "text": "[server]\nport = 8080\n"
}

Returns the diff block showing every prior line removed and the new lines minted with fresh anchors.
