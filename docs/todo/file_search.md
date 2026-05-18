# File Search Tool

## Motivation

Agents reach for Bash (`find`, `rg`, `ls`) far more than a dedicated file-search tool, because the dedicated tools available today are too narrow to compete with shell composition. The goal is one tool that handles "questions about files" well enough that Bash is genuinely reserved for "I need to run a thing."

### Why agents skip the existing Glob-style tool

- Returns paths only, sorted by mtime. Almost every use leads to a second tool call (Read or Grep) anyway, so Bash chains win.
- Grep is usually what's actually wanted — "files containing Y" rather than "files named X."
- mtime sort is surprising; alphabetical is the expected default.
- No gitignore, no exclusions. `**/*.rs` drags in `target/`, `node_modules/`, vendored deps.
- No composition. Bash pipes into head, wc, xargs trivially; the tool is one-pattern-in, one-list-out.
- Reflex: when an agent thinks "find files," fingers type `rg --files` or `find`. Nothing in the tool's description tells the agent it would lose anything by reaching for Bash.

## Design

One tool covering name-pattern lookup, content search, and directory listing, with structured output and variable integration.

### Inputs

- `paths`: list of glob patterns to include. Optional.
- `search`: content pattern (regex). Optional.
- `exclude`: list of glob patterns to exclude. Optional.
- `ignore`: bool, default `true`. Honors `.gitignore`, `.ignore` and `.rgignore`.
- `hidden`: bool, default `false`. Separate from gitignore — hidden files like `.env.example` are a different concern from vendored/ignored trees.
- `depth`: int, optional. Unset = unlimited.
- `paths_only`: bool, default `false`. When `search_text` is given, returns paths only without per-match detail.
- `into`: string, optional. Variable name to dump the full structured result into.

#### Argument validity

- `paths` and `search` are both optional. At least one of: `paths`, `search`, or no-args may be supplied.
- No-args is **valid** and means "recursive listing of pwd, gitignore on" — equivalent to `paths: ["**/*"]`. This matches the common orientation move when an agent enters an unfamiliar repo.
- Empty `paths: []` with no `search` is an **error**, not "everything." Empty list is different from omitted, and silently treating `[]` as a wildcard is a footgun for agents building args programmatically.
- Error messages must be blunt: `provide at least one of "paths" or "search", or call with no arguments`. Vague "invalid arguments" wastes a turn.

### Pattern syntax

Use ripgrep's glob dialect (`**`, `!negation`, `{a,b}`). Document it explicitly in the tool description with two examples. Agents fail glob patterns constantly because every tool's dialect is subtly different — picking one and stating it matters more than picking the "right" one.

### Output

Each result entry:

- `path`
- `size` (bytes)
- `mtime` (ISO 8601)
- `binary: bool` — so the agent knows to skip reading it
- If `search` given and not `paths_only`:
  - `match_count`
  - `first_match`: `{ line: int, text: string }` — the matching line itself, no surrounding context by default

Sort: alphabetical by path. **No relevance ranking.** Ranking is hard, surprising when wrong, and agents handle sorted lists better than opaque scoring.

### Inline summary vs. variable dump

When `input` is set:

- Write the full structured result to the variable.
- **Also** return an inline summary alongside the variable write — count plus the first ~5 paths (or first 5 matches). If the only output is "stored as $files," the agent must make a second call to peek, which destroys the ergonomics.

When `input` is unset:

- Return results inline.

### Truncation

Loud, never silent. If a cap is hit:

- Include the actual count if known, or `N+` if not.
- Phrase it so the agent recognizes it as a signal to narrow the query: `1000+ matches, capped at 1000 — narrow paths or search_text to see all`.

### Description copy

Tell the agent in the tool description:

- This is ripgrep under the hood (assuming it is). Tells the agent when behavior will be predictable and when it might surprise.
- Respects `.gitignore`, `.ignore`, `.rgignore` by default.
- The glob dialect being used.
- Two worked examples covering the common shapes:
  - `paths` only ("what Rust files are here")
  - `paths` + `search` ("which configs mention DATABASE_URL")

## Decisions made

- **One tool, not three.** Collapse find/grep/ls into one surface so the agent's mental model is "questions about files" → this tool, "run a thing" → Bash. Don't ship a separate LS tool alongside.
- **Recursive default for no-args**, not one-level. Matches `paths: ["**/*"]`; one-level is rarely needed for pwd specifically (when wanted, it's usually for a subdir, expressible as `paths: ["subdir/*"]`).
- **Alphabetical sort by default**, not mtime, not relevance.
- **No `context` lines by default** around matches. The matching line plus its line number is enough for triage; full context is one Read call away and noisy in aggregate.

## Explicitly rejected

- **Relevance ranking on `search` results.** Sort alphabetically.
- **Inventing a glob dialect.** Pick rg's and document.
- **Single-pattern `paths` / `exclude`.** Must be lists.
- **Silent `[]` = everything fallback.** Error instead.
- **Surrounding context lines by default.** Opt-in if added later.

## Open questions

- Whether to also surface a cheap language guess per file (e.g. from extension). Probably skip — the path already tells you, and a wrong guess is worse than no guess.
- Cap value for results. Needs to be high enough that real exploration works (likely 500–2000) but low enough that a runaway query doesn't blow the variable budget.
- Whether `max_depth` should have a non-unlimited default in very large repos. Probably no — gitignore handles most of the size problem, and unlimited matches the principle of least surprise.
