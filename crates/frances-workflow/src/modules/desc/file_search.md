Search the filesystem by name pattern, by content, or both.

Backed by the ripgrep crates (`ignore::WalkParallel` + `grep-searcher`). Respects `.gitignore`, `.ignore`, and `.rgignore` by default; toggle with `ignore: false`. Hidden files (dotfiles, dot-dirs) are excluded by default; set `hidden: true` to include them. Paths are resolved against the client's working directory unless absolute.

Args: `{ paths?, search?, exclude?, ignore?, hidden?, depth?, paths_only?, into? }`

  paths: list of include globs (ripgrep dialect — `**` for recursive, `{a,b}` for alternation). Omit to match everything under cwd.
  search: optional regex (Rust regex dialect). When set, results are filtered to files containing at least one match. Binary files are skipped.
  exclude: list of globs to subtract from `paths` (same dialect).
  ignore: bool, default `true`. When `false`, `.gitignore`/`.ignore`/`.rgignore` are not consulted.
  hidden: bool, default `false`. When `true`, hidden files and directories are included.
  depth: optional integer max-depth. Omit for unbounded.
  paths_only: bool, default `false`. When `search` is given, suppresses `first_match` but keeps `match_count` per file (useful for ranking by hit density).
  into: optional Frances variable name. When set, the full structured result lands in that variable AND an inline summary (count + first 5 paths/matches) is returned.

Empty `paths: []` with no `search` is an error. Call with no arguments to get a recursive listing of the cwd.

Result entries are sorted alphabetically by path. Each entry is `{ path, size, mtime, binary, match_count?, first_match? }`. The output is capped at 1000 entries; if more would have matched, a `truncated` field is included so you know to narrow the query.

Examples:

  // What Rust files are here?
  { paths: ["**/*.rs"] }

  // Which configs mention DATABASE_URL?
  { paths: ["**/*.toml", "**/*.yaml", "**/*.json"], search: "DATABASE_URL" }

  // Stash the full result of a regex sweep into a variable, get an inline preview.
  { search: "TODO\\(.+\\):", into: "todos" }
