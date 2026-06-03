Create a new file with the given content.

Args: `{ path, text }`

  path:  the file to create. Must NOT already exist on disk — for an existing file use `file_overwrite` instead (which requires a prior `file_read` so you've seen the content you're replacing).

You do NOT need to call `file_read` first — the file doesn't exist yet. The response echoes the file back with fresh anchors on every line, so subsequent `file_replace_lines` / `file_insert_after` / `file_insert_before` calls against the same path can use those anchors directly without a separate read.

WORKED EXAMPLE.

{
  "path": "src/notes.md",
  "text": "# Notes\n\nfirst draft\n"
}

Returns the new file rendered as `Word§content` lines (one per source line).

