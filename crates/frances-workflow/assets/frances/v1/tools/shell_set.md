Export a Frances variable into bash as an environment variable. The value persists: subsequent `shell_run` calls see it, and their subprocesses (e.g. `./script.sh`) inherit it.

Args: `{ name, from }`

  name: bash variable name to assign and export (`[A-Za-z_][A-Za-z0-9_]*`). FRANCES_ROOT is reserved and Frances-managed.
  from: Frances variable name to pull the value from. Strings pass through verbatim; objects, arrays, numbers, booleans, and `null` are JSON-encoded.

Mechanism: the value is written to a temp file and bash runs `export <name>=$(cat 'tmpfile')`, so multi-line / special-character payloads survive intact. Note that `$(cat …)` strips a trailing newline — if I need an exact byte sequence including trailing `\n`, I prefer pulling the value into bash some other way.

The response is a short summary of what got bound (no value echo). Call `shell_run` afterwards to use the bash variable.
