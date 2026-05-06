# Daemon, session, TTY

The unit of state is a **session**, identified by a random ID. A session owns:

- `state_root/sessions/<id>/` — durable: `metadata.bin`, `frances.db` (turso), anchor state.
- `runtime_root/sessions/<id>/` — ephemeral: Unix sockets (`control`, `client`, `events`), pid file, log.
- `runtime_root/tty-links/<tty_key>` — symlink from the controlling TTY to its session dir.

`state_root` resolves from `XDG_STATE_HOME` (else `~/.local/state/frances`); `runtime_root` from `XDG_RUNTIME_DIR` (else `/tmp/frances-<uid>`). Both directories are created `0700`.

One daemon per session. `daemon::spawn::ensure_daemon` either confirms a healthy daemon or spawns one by re-execing the same binary with `--daemon <id>`. The client attaches over `control_socket`, then issues prompts over `client_socket` while reading `events_socket` for streaming output.

`OPENROUTER_API_KEY` must be set in the environment that invokes the client; the client forwards its env to the daemon at attach time, so the daemon picks the key up from the attaching invocation.

## Protocol versioning

Protocol version is a build-time random u64. `crates/frances/build.rs` writes `protocol_id.rs` from `cwd + unix_time + /dev/urandom`. Client and daemon must come from the same build — a stale daemon from a previous build is detected on the banner, stopped, and respawned. Don't try to make the protocol stable across builds; it's intentionally per-build.

## Per-session database

Each session has its own `frances.db` inside its session dir. The schema (`messages`, `blocks`, anchor tables) has **no `session_id` columns** — sessions are isolated at the file level, not by row. Don't reintroduce a global db at `state_root/frances.db`: turso uses exclusive file locks and that caused cross-daemon contention. `Session::database_path()` is the source of truth.
