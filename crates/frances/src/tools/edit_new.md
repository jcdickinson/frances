Create a new file with the given content.

Args: `{ path, text }`

  path:  the file to create. Must NOT already exist on disk — for an existing file use `edit_overwrite` instead (which requires a prior `read_file` so you've seen the content you're replacing).
  text:  the file's content. Use `\n` for newlines.

You do NOT need to call `read_file` first — the file doesn't exist yet. The response echoes the file back with fresh anchors on every line, so subsequent `edit_replace` / `edit_insert_after` / `edit_insert_before` calls against the same path can use those anchors directly without a separate read.

After every edit the file is run through the project formatter and written to disk; the returned content reflects the post-format result.

WORKED EXAMPLE.

{
  "path": "src/notes.md",
  "text": "# Notes\n\nfirst draft\n"
}

Returns the new file rendered as `Word§content` lines (one per source line).
