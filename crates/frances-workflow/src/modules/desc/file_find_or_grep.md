Find files by name pattern and/or grep their contents. The one tool for any "where are the files," "what files match this glob," or "which files mention X" question — pick it instead of running `ls`, `find`, `tree`, `rg`, or `grep -r` in the shell.

Backed by the ripgrep crates (`ignore::WalkParallel` + `grep-searcher`). Respects `.gitignore`, `.ignore`, and `.rgignore` by default; toggle with `ignore: false`. Hidden files (dotfiles, dot-dirs) are excluded by default; set `hidden: true` to include them. Paths are resolved against the client's working directory unless absolute.

Shell commands this replaces:

  `ls` / `ls -la`              → `{ depth: 1 }`
  `ls some/dir`                → `{ paths: ["some/dir/*"], depth: 1 }`
  `ls -la` (with dotfiles)     → `{ depth: 1, hidden: true }`
  `tree -L 2`                  → `{ depth: 2 }`
  `find . -name "*.rs"`        → `{ paths: ["**/*.rs"] }`
  `find . -name "*config*"`    → `{ paths: ["**/*config*"] }`
  `find src -name "*.ts"`      → `{ paths: ["src/**/*.ts"] }`
  `rg --files`                 → `{}`
  `rg "pattern"`               → `{ search: "pattern" }`
  `grep -r "pattern" .`        → `{ search: "pattern" }`
  `rg "TODO" -g "*.rs"`        → `{ paths: ["**/*.rs"], search: "TODO" }`

Args: `{ paths?, search?, exclude?, ignore?, hidden?, depth?, paths_only?, into? }`

  paths: list of include globs (ripgrep dialect — `**` for recursive, `{a,b}` for alternation). Omit to match everything under cwd.
  search: optional regex (Rust regex dialect). When set, results are filtered to files containing at least one match. Binary files are skipped.
  exclude: list of globs to subtract from `paths` (same dialect).
  ignore: bool, default `true`. When `false`, `.gitignore`/`.ignore`/`.rgignore` are not consulted.
  hidden: bool, default `false`. When `true`, hidden files and directories are included.
  depth: optional integer max walk depth. Omit for unbounded. Depth counts from each starting point: `depth: 0` includes only the starting path itself, `depth: 1` includes its immediate children, `depth: 2` includes grandchildren, etc. Use `depth: 1` to list files directly in the cwd.
  paths_only: bool, default `false`. When `search` is given, suppresses `first_match` but keeps `match_count` per file (useful for ranking by hit density).
  into: optional Frances variable name. When set, the full structured result lands in that variable AND an inline summary (count + first 5 paths/matches) is returned.

Omitting both `paths` and `search` is valid — `{}`, `{ depth: 1 }`, `{ depth: 1, ignore: false }`, `{ hidden: true }`, etc. all list files under cwd subject to whatever filters you set. The only rejected shape is an explicit `paths: []` with no `search` — an empty list is treated as a likely bug, not a wildcard; send `{}` if you really mean "everything."

Result entries are sorted alphabetically by path. Each entry is `{ path, size, mtime, binary, match_count?, first_match? }`. The output is capped at 1000 entries; if more would have matched, a `truncated` field is included so you know to narrow the query.

Examples:

  // List cwd (like `ls`)
  { depth: 1 }

  // List a subdirectory non-recursively
  { paths: ["docs/*"], depth: 1 }

  // What Rust files are here? (like `find . -name "*.rs"`)
  { paths: ["**/*.rs"] }

  // Find by name pattern
  { paths: ["**/*config*"] }

  // All TODO comments under src/ (like `rg TODO src/`)
  { paths: ["src/**/*.rs"], search: "TODO" }

  // Which configs mention DATABASE_URL?
  { paths: ["**/*.toml", "**/*.yaml", "**/*.json"], search: "DATABASE_URL" }

  // Stash the full result of a regex sweep into a variable, get an inline preview.
  { search: "TODO\\(.+\\):", into: "todos" }
