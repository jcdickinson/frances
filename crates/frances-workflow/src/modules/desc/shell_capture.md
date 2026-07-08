Pull the current value of a bash variable into a Frances variable.

Args: `{ name, from }`

  name:  Frances variable name to store the captured value into.
  from:  bash variable name to read (`[A-Za-z_][A-Za-z0-9_]*`).

Mechanism: bash runs `( set -u; printf '%s' "$<from>" > 'tmpfile' )` and Rust reads the file back. The captured value is always stored as a string in Frances; I use `variable_assign` with `filter: "fromjson"` if I know the content is JSON-encoded.

Errors if the bash variable is unset (the `set -u` subshell makes "unset" distinguishable from "empty"). To capture command output instead of a variable, redirect with `shell_run` first (e.g. `RESULT=$(some-cmd)`) then `shell_capture` from `RESULT`.

The response reports only the captured size as a type summary (e.g. `x = string`) — no value echo.
