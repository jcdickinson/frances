Pull the current value of a bash variable into a Frances variable.

Args: `{ name, from }`

  name:  Frances variable name to store the captured value into.
  from:  bash variable name to read (`[A-Za-z_][A-Za-z0-9_]*`).

Mechanism: bash runs `( set -u; printf '%s' "$<from>" > 'tmpfile' )` and Rust reads the file back. The captured value is always stored as a string in Frances; I use `variable_assign` with `filter: "fromjson"` if I know the content is JSON-encoded.

Errors if the bash variable is unset (the `set -u` subshell makes "unset" distinguishable from "empty"). Each run is a fresh bash, so only persisted environment variables survive between calls: to capture command output, run `export RESULT=$(some-cmd)` via `shell_run` with `persist: ["RESULT"]`, then `shell_capture` from `RESULT`.

The response reports only the captured size as a type summary (e.g. `x = string`) — no value echo.
