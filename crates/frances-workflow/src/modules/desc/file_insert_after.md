Insert new content immediately after a specific line in a file.

Args: `{ path, anchor, text }`

  path:    the file to edit. Must have been read this turn via `file_read` (or just created with `file_new` / `file_overwrite`, which echo back anchors).
  anchor:  full rendered anchor line that the new content will be inserted AFTER — `Word§content`, exactly as `file_read` produced it.
  text:    the raw content of the new line(s) and NOTHING ELSE. Use `\n` for newlines. Multi-line is fine.

CRITICAL: `text` must NEVER contain a `Word§` prefix unless you genuinely want those literal characters in the file (editing the anchor engine itself, a test fixture, prose with `Word§`). Anchors are read-only metadata the engine assigns — they're not part of line content. If you paste back the rendered prefixes from a `file_read`, they get written verbatim and your edit is broken.

  WRONG → text: "Apple§def hello():\nBanana§    print(\"hi\")"
  RIGHT → text: "def hello():\n    print(\"hi\")"

Provide exactly one of `text` or `from`:

  from:    a Frances variable name. Its value is used as the insertion text (string values pass through verbatim; non-string values are JSON-encoded). Use this when the content was prepared via `variable_set` / `variable_assign` / `file_read into:` / `shell_capture`, to avoid re-emitting a long payload in a tool-call.

Anchor protocol: every line in a `file_read` (or post-edit diff) is rendered as `Word§content`. The `Word` half identifies the line by anchor; the content half is what's currently on that line. Pass back both halves verbatim — the engine splits on the first `§`, validates the anchor word against the cached file, and compares the content (trimmed) for safety. On a content mismatch the call fails and you should re-read the file before retrying.

After every edit the file is run through the project formatter and written to disk; the returned diff block reflects the post-format content.

WORKED EXAMPLE. After `file_read` on `src/greet.py` returns:

  Apple§def hello():
  Banana§    print("hi")
  Daisy§def goodbye():

To insert a docstring before `goodbye` (which is the same as inserting AFTER the blank line — but if there isn't one, you can pin to `Banana` and insert after that, or use `file_insert_before` on `Daisy`). To add a comment after the print:

{
  "path": "src/greet.py",
  "anchor": "Banana§    print(\"hi\")",
  "text":   "    # printed greeting"
}

Returns the diff block for the file with new anchors for inserted lines.
