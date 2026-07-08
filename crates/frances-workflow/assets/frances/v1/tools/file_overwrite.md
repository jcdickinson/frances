Replace the entire content of an existing file.

Args: `{ path, text }`

  path:  the file to overwrite. I MUST have called `file_read` on this path this turn — overwrite is destructive, so the engine requires me to have seen the prior content first.

I use this when I would otherwise need many overlapping line edits, or when I am rewriting a file from scratch. For creating a brand-new file, I use `file_new` instead. For surgical changes, I prefer `file_replace_lines` / `file_insert_after` / `file_insert_before`.

The response echoes the post-write file back with fresh anchors on every line (the prior anchors are tombstoned), so subsequent `file_replace_lines` / `edit_insert_*` calls against the same path can use the new anchors directly without re-reading.

WORKED EXAMPLE.

{
  "path": "src/config.toml",
  "text": "[server]\nport = 8080\n"
}

Returns the diff block showing every prior line removed and the new lines minted with fresh anchors.
