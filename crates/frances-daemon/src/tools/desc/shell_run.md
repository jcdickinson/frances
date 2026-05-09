Run a bash command in a long-lived shell. State persists across calls — environment variables, current directory (cd), shell functions, sourced scripts, and `set` flags carry into the next call. The bash code you submit is sourced as-is: write multi-line scripts, pipelines, subshells, heredocs, function definitions, redirections, etc. Do NOT wrap in `bash -c '...'` or escape quotes — pass bash as you would type it interactively.

Returns one of:
  [exit N]\n<output>                          — finished with exit code N (stdout+stderr merged in order).
  [still running — <reason>]\n<partial>       — wait window expired; call shell_wait to continue, or shell_kill to abort.
  [shell died]\n<final>                        — bash itself exited (e.g. you ran `exit`). Next shell_run spawns a fresh shell, losing state.

Optional `quiet_ms` (default 1000) returns 'still running' after that many ms of output silence — the timer resets every time bytes arrive. Optional `max_ms` (no default) returns 'still running' after that wall-clock regardless of streaming. quiet_ms=0 disables silence detection. Use max_ms to bound how long this single tool call blocks.

Neither `quiet_ms` nor `max_ms` kills the command — they just yield control back to you with the output so far while the command keeps running. Use shell_kill if you actually want to terminate it.

When you get 'still running', do NOT write prose narrating the wait ("the build is still going…"). Just call shell_wait (or shell_kill) immediately as your next tool call. Save tokens for actual progress.

Interactive apps that hard-require a TTY (vim, top, psql without -c) are NOT supported. Use their non-interactive equivalents (psql -c "SELECT 1", ssh host cmd).
