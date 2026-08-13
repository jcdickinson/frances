# Session runtime

The unit of state is a **session**, identified by a random ID. A session owns:

- `state_root/sessions/<id>/` — durable: `metadata.bin`, `frances.db` (turso), anchor state, `frances.log`.
- `runtime_root/sessions/<id>/` — ephemeral runtime dir. Empty in the current
  build; reserved for future per-process locks or scratch files.
- `runtime_root/tty-links/<tty_key>` — symlink from the controlling TTY to
  its session dir.

`state_root` resolves from `XDG_STATE_HOME` (else `~/.local/state/frances`);
`runtime_root` from `XDG_RUNTIME_DIR` (else `/tmp/frances-<uid>`). Both
directories are created `0700`.

## In-process runtime

There is no daemon. `frances` is a single process: the binary captures the
controlling TTY, resolves (or creates) the matching session, opens the
per-session turso database, and constructs a
[`SessionRuntime`](../../crates/frances-session/src/runtime/mod.rs). The desktop UI
runs in the same process and talks to the runtime through:

- `SessionRuntime::prompt(text)` — spawns the workflow cycle.
- `SessionRuntime::respond_permission(id, response)` — settles a pending
  permission.
- An `mpsc::UnboundedReceiver<StreamFrame>` paired with the runtime's
  [`EventsChannel`](../../crates/frances-session/src/runtime/events.rs) — the
  UI drains scrollback replay, prompt frames, and any workflow-switch
  replay through this channel.

`OPENROUTER_API_KEY` and any other secrets come from the process environment
at startup; the runtime carries an `InvocationContext` snapshot of env + cwd
that workflows read via `current_env` / `current_cwd`.

There is no IPC, no socket-pairing race, no protocol versioning, no
re-attach. The single-process model intentionally drops the "session
outlives the UI" property — quitting the app cancels any in-flight turn.
Persisted state (scrollback rows, history rows, workflow metadata) is
written eagerly during the turn so a partial turn survives the restart.

## Per-session database

Each session has its own `frances.db` inside its session dir. The schema
(`messages`, `blocks`, anchor tables) has **no `session_id` columns** —
sessions are isolated at the file level, not by row. Don't reintroduce a
global db at `state_root/frances.db`: turso uses exclusive file locks and
that caused cross-process contention back when sessions were owned by
separate daemons. `Session::database_path()` is the source of truth.
