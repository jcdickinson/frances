Replace one contiguous range of lines in a file with new content.

Args: `{ path, anchor, end_anchor, text }`

  path:        the file to edit. Must have been read this turn via `file_read` (or just created with `file_new` / `file_overwrite`, which echo back anchors).
  anchor:      full rendered anchor line of the FIRST line in the range — `Word§content`, exactly as `file_read` produced it.
  end_anchor:  full rendered anchor line of the LAST line in the range (inclusive). For a single-line replace, pass the same value as `anchor`.
  text:        the replacement content. Use `\n` for newlines. Multi-line is fine; do NOT include any anchors in `text`.

Provide exactly one of `text` or `from`:

  from:        a Frances variable name. Its value is used as the replacement text (string values pass through verbatim; non-string values are JSON-encoded). Use this when the content was prepared via `variable_set` / `variable_assign` / `file_read into:` / `shell_capture`, to avoid re-emitting a long payload in a tool-call.

Anchor protocol: every line in a `file_read` (or post-edit diff) is rendered as `Word§content`. The `Word` half identifies the line by anchor; the content half is what's currently on that line. Pass back both halves verbatim — the engine splits on the first `§`, validates the anchor word against the cached file, and compares the content (trimmed) for safety. On a content mismatch the call fails and you should re-read the file before retrying.

After every edit the file is run through the project formatter and written to disk; the returned diff block reflects the post-format content.

WORKED EXAMPLE. After `file_read` on `src/greet.py` returns:

  Apple§def hello():
  Banana§    print("hi")
  Cherry§
  Daisy§def goodbye():

To replace the print with two prints:

{
  "path": "src/greet.py",
  "anchor":     "Banana§    print(\"hi\")",
  "end_anchor": "Banana§    print(\"hi\")",
  "text":       "    print(\"hi there\")\n    print(\"welcome\")"
}

Returns the diff block for the file with new anchors for inserted lines.
