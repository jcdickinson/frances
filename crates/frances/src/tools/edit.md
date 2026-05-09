Edit one or more files. Five edit types are supported:

  replace        — replaces all lines from `anchor` through `end_anchor` (inclusive) with `text`.
  insert_after   — inserts `text` immediately after `anchor`.
  insert_before  — inserts `text` immediately before `anchor`.
  new            — creates a new file with `text` as its content. Fails if the file already exists.
  overwrite      — replaces an existing file's content with `text`. Requires a fresh `read_file` for that path this turn.

For `replace`, `insert_after`, and `insert_before`, you must call `read_file` on each file first this turn so its anchors are cached. `new` does not need a read. `overwrite` does — it's the safety net so you've actually seen the prior content before throwing it away.

After every edit (any type) the file is run through the project formatter and written to disk; the diff block returned reflects the post-format content.

A single edit call can create and/or modify many files at once — add one entry per file under `files`. You can freely mix `new`, `overwrite`, and line-level edits across different files in the same call. The only constraint is that any path using `new` or `overwrite` must appear exactly once in the `files` array (no duplicate entries for the same path).

Top-level shape — `files` is an array of file objects; each file has an `edits` array:

{
  "files": [
    {
      "path": "src/example.py",
      "edits": [
        { "edit_type": "replace", "anchor": "...", "end_anchor": "...", "text": "..." },
        { "edit_type": "insert_after", "anchor": "...", "text": "..." }
      ]
    }
  ]
}

Per-edit fields:
  edit_type:  one of "replace", "insert_after", "insert_before", "new", "overwrite"
  anchor:     full anchor line as `read_file` rendered it — "Word§content". Required for replace/insert_*; ignored for new/overwrite.
  end_anchor: only for replace; the rendered anchor line of the LAST line in the inclusive range.
  text:       the new content. Use \n for newlines. Multi-line is fine; do NOT include any anchors in text.

The anchor word must match a line in the latest `read_file` output for that path. The content after § must match the line's content (trimmed comparison). On mismatch, re-read the file and use the latest anchors.

`new` and `overwrite` are whole-file operations. They must be the ONLY edit in their file's `edits` array — do not mix them with line-level edits for the same path. (Mixing across different paths is fine; uniqueness applies per-path.)

Edits within a single call must not touch overlapping line ranges in the same file. If they do the call is rejected — split overlapping work into separate calls.

WORKED EXAMPLE. Suppose read_file on src/greet.py returned:

  Apple§def hello():
  Banana§    print("hi")
  Cherry§
  Daisy§def goodbye():

To replace the print with two prints AND add a docstring before goodbye, the WHOLE tool call body is:

{
  "files": [
    {
      "path": "src/greet.py",
      "edits": [
        {
          "edit_type": "replace",
          "anchor":     "Banana§    print(\"hi\")",
          "end_anchor": "Banana§    print(\"hi\")",
          "text":       "    print(\"hi there\")\n    print(\"welcome\")"
        },
        {
          "edit_type": "insert_before",
          "anchor": "Daisy§def goodbye():",
          "text":   "# Says goodbye."
        }
      ]
    }
  ]
}

To create a new file:

{
  "files": [
    {
      "path": "src/notes.md",
      "edits": [
        { "edit_type": "new", "text": "# Notes\n\nfirst draft\n" }
      ]
    }
  ]
}

Returns one diff block per file with the new anchors for inserted lines.
