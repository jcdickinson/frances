Replace every regex match in one previously-read file.

Args: `{ path, find, replacement, count? }`

  path:        The file to edit. Must have been read this turn via `file_read` (or just created with `file_new` / `file_overwrite`, which register the file for editing).
  find:        A Rust `regex` crate pattern. Rust regexes do not support lookaround or backreferences in the pattern.
  replacement: Replacement template using Rust regex replacement syntax: `$1`, `${name}`, and `$$` for a literal `$`.
  count:       Optional maximum match count. If `find` matches more than `count` times, the call fails without writing and reports the actual match count.

This tool is unanchored by design: it is for cross-cutting changes such as renaming a repeated identifier or normalizing a recurring import. I use `file_replace_lines` when I need a precise anchor-bracketed range.

If the regex matches zero times, the call succeeds with a no-changes result; it does not write the file or run the formatter.

On a real change, the file is written through the normal edit pipeline: the project formatter runs, the file is re-anchored, and the returned content is the same kind of diff block produced by `file_replace_lines`.

WORKED EXAMPLE. After `file_read` has registered `src/app.js`, rename every `oldName` identifier to `newName`, but fail safely if the pattern is too broad:

{
  "path": "src/app.js",
  "find": "\\boldName\\b",
  "replacement": "newName",
  "count": 20
}

Capture groups may be referenced from the replacement:

{
  "path": "src/routes.rs",
  "find": "route_(\\w+)",
  "replacement": "handler_$1"
}
