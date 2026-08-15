Read the current value of a bash variable into a Frances variable.

Args: `{ name, from }`

  name:  Frances variable name to store the value into.
  from:  bash variable name to read (`[A-Za-z_][A-Za-z0-9_]*`).

Mechanism: bash runs `( set -u; printf '%s' "$<from>" > 'tmpfile' )` and Rust reads the file back. The value is always stored as a string in Frances; I use `var_edit` with `filter: "fromjson"` if I know the content is JSON-encoded.

Errors if the bash variable is unset (the `set -u` subshell makes "unset" distinguishable from "empty"). Each run is a fresh bash, so only persisted environment variables survive between calls: to capture command output, run `export RESULT=$(some-cmd)` via `shell_run` with `persist: ["RESULT"]`, then `shell_get` from `RESULT`.

The response reports only a type summary (e.g. `x = string`) — no value echo.
