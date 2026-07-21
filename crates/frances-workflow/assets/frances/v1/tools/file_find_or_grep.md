Find files by name pattern and/or grep their contents. The one tool for any "where are the files," "what files match this glob," or "which files mention X" question — pick it instead of running `ls`, `find`, `tree`, `rg`, or `grep -r` in the shell.

Backed by the ripgrep crates (`ignore::WalkParallel` + `grep-searcher`). Respects `.gitignore`, `.ignore`, and `.rgignore` by default; toggle with `ignore: false`. Hidden files (dotfiles, dot-dirs) are excluded by default; set `hidden: true` to include them. Paths are resolved against the client's working directory (or `root` if provided).


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

Args: `{ root?, paths?, search?, exclude?, ignore?, hidden?, depth?, paths_only?, into? }`

  root: optional string. Changes the walk root directory. By default the walk starts at the client's working directory; I set `root` to search a different tree (e.g. a dependency source directory). Supports `~` expansion (e.g. `"~/.cargo/registry/..."`). Relative roots are resolved against cwd. The root must exist and be a directory. `paths` globs are matched relative to `root`. When searching outside the project, I will typically also want `ignore: false` and/or `hidden: true`.

  paths: list of include globs (ripgrep dialect — `**` for recursive, `{a,b}` for alternation). Omit to match everything under cwd.
  search: optional regex (Rust regex dialect). When set, results are filtered to files containing at least one match. Binary files are skipped.
  exclude: list of globs to subtract from `paths` (same dialect).
  ignore: bool, default `true`. When `false`, `.gitignore`/`.ignore`/`.rgignore` are not consulted.
  hidden: bool, default `false`. When `true`, hidden files and directories are included.
  depth: optional integer max walk depth. Omit for unbounded. Depth counts from each starting point: `depth: 0` includes only the starting path itself, `depth: 1` includes its immediate children, `depth: 2` includes grandchildren, etc. Use `depth: 1` to list files directly in the cwd.
  paths_only: bool, default `false`. When `search` is given, suppresses `first_match` but keeps `match_count` per file (useful for ranking by hit density).
  into: optional Frances variable name. When set, the full structured result lands in that variable AND an inline summary (count + first 5 paths/matches) is returned.

Omitting both `paths` and `search` is valid — `{}`, `{ depth: 1 }`, `{ depth: 1, ignore: false }`, `{ hidden: true }`, etc. all list files under cwd subject to whatever filters I set. The only rejected shape is an explicit `paths: []` with no `search` — an empty list is treated as a likely bug, not a wildcard; I send `{}` if I really mean "everything."

Result entries are sorted alphabetically by path. Each entry is `{ path, size, mtime, binary, match_count?, first_match? }`. Matching-line text is limited to a 512-byte excerpt around the match; oversized lines set `first_match.text_truncated` and `first_match.line_bytes`. The structured result is capped at 1000 entries, and the inline response is capped at 16 KiB. Either limit reports its omission explicitly so I know to narrow the query.

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

Searching outside the project tree:

  // Find a crate's source in the cargo registry
  { root: "~/.cargo/registry/src", paths: ["**/serde*/**/*.rs"], ignore: false }

  // Grep for a function definition in a dependency
  { root: "~/.cargo/registry/src/index.crates.io-*/serde-1.0.215", search: "fn deserialize", ignore: false }

  // Search a sibling project (relative root)
  { root: "../other-project", paths: ["**/*.ts"], search: "export.*fetch" }

  // Important: `ignore` and `hidden` defaults still apply under external roots.
  // Use `ignore: false` to bypass .gitignore, `hidden: true` to include dotfiles.
