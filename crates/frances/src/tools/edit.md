Edit one or more files by replacing, inserting after, or inserting before specific anchored lines. You must call `read_file` on each file first this turn so its anchors are cached.

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
  edit_type:  one of "replace", "insert_after", "insert_before"
  anchor:     full anchor line as `read_file` rendered it — "Word§content"
  end_anchor: only for replace; the rendered anchor line of the LAST line in the inclusive range
  text:       the new content. Use \n for newlines. Multi-line is fine; do NOT include any anchors in text.

The anchor word must match a line in the latest `read_file` output for that path. The content after § must match the line's content (trimmed comparison). On mismatch, re-read the file and use the latest anchors.

Behaviour:
  replace        — replaces all lines from `anchor` through `end_anchor` (inclusive) with `text`.
  insert_after   — inserts `text` immediately after `anchor`.
  insert_before  — inserts `text` immediately before `anchor`.

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

Returns one diff block per file with the new anchors for inserted lines.
