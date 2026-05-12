Bind a Frances variable to a bash variable. Subsequent `shell_run` calls see it.

Args: provide exactly one of `set` or `export` (which determines visibility), plus `from`:

  set:    bash variable name to assign as a plain shell variable. Visible to the current bash session only — child processes invoked from `shell_run` do NOT inherit it.
  export: bash variable name to assign AND export. Same as `set` but also `export`-ed so subprocesses spawned by `shell_run` (e.g. `./script.sh`) see it in their environment.
  from:   Frances variable name to pull the value from. Strings pass through verbatim; objects, arrays, numbers, booleans, and `null` are JSON-encoded.

Bash names must match `[A-Za-z_][A-Za-z0-9_]*`.

Mechanism: the value is written to a temp file and bash runs `<name>=$(cat 'tmpfile')` (or `export <name>=…` for the `export` form), so multi-line / special-character payloads survive intact. Note that `$(cat …)` strips a trailing newline — if you need an exact byte sequence including trailing `\n`, prefer pulling the value into bash some other way.

The response is a short summary of what got bound (no value echo). Call `shell_run` afterwards to use the bash variable.
