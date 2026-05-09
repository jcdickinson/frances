Insert new content immediately before a specific line in a file.

Args: `{ path, anchor, text }`

  path:    the file to edit. Must have been read this turn via `read_file` (or just created with `edit_new` / `edit_overwrite`, which echo back anchors).
  anchor:  full rendered anchor line that the new content will be inserted BEFORE — `Word§content`, exactly as `read_file` produced it.
  text:    the content to insert. Use `\n` for newlines. Multi-line is fine; do NOT include any anchors in `text`.

Anchor protocol: every line in a `read_file` (or post-edit diff) is rendered as `Word§content`. The `Word` half identifies the line by anchor; the content half is what's currently on that line. Pass back both halves verbatim — the engine splits on the first `§`, validates the anchor word against the cached file, and compares the content (trimmed) for safety. On a content mismatch the call fails and you should re-read the file before retrying.

After every edit the file is run through the project formatter and written to disk; the returned diff block reflects the post-format content.

WORKED EXAMPLE. After `read_file` on `src/greet.py` returns:

  Apple§def hello():
  Banana§    print("hi")
  Daisy§def goodbye():

To add a comment before `goodbye`:

{
  "path": "src/greet.py",
  "anchor": "Daisy§def goodbye():",
  "text":   "# Says goodbye."
}

Returns the diff block for the file with new anchors for inserted lines.
