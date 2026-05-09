Continue the bash command that previously returned 'still running'. Same return shape as shell_run. Optional `quiet_ms` and `max_ms` work the same. Errors if no command is currently in flight.

If you just need to wait longer, call this immediately — no narration between calls. Pick a `quiet_ms` matched to how long the command typically goes between outputs; long-running compilations or test suites can be silent for tens of seconds at a time.
