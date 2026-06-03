Replace the entire content of an existing file.

Args: `{ path, text }`

  path:  the file to overwrite. You MUST have called `file_read` on this path this turn — overwrite is destructive, so the engine requires you to have seen the prior content first.

Use this when you'd otherwise need many overlapping line edits, or when you're rewriting a file from scratch. For creating a brand-new file, use `file_new` instead. For surgical changes, prefer `file_replace_lines` / `file_insert_after` / `file_insert_before`.

The response echoes the post-write file back with fresh anchors on every line (the prior anchors are tombstoned), so subsequent `file_replace_lines` / `edit_insert_*` calls against the same path can use the new anchors directly without re-reading.

WORKED EXAMPLE.

{
  "path": "src/config.toml",
  "text": "[server]\nport = 8080\n"
}

Returns the diff block showing every prior line removed and the new lines minted with fresh anchors.

