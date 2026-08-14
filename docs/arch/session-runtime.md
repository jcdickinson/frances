# Session runtime

The unit of state is a **session**, identified by a random ID. A session owns:

- `state_root/sessions/<id>/` — durable: `metadata.bin`, `frances.db` (turso), anchor state, `frances.log`.
- `runtime_root/sessions/<id>/` — ephemeral runtime dir. Empty in the current
  build; reserved for future per-process locks or scratch files.

`state_root` resolves from `XDG_STATE_HOME` (else `~/.local/state/frances`);
`runtime_root` from `XDG_RUNTIME_DIR` (else `/tmp/frances-<uid>`). Both
directories are created `0700`.

## Workspaces

A launch opens a **workspace**: `frances [path]` where path is a directory
(an implicit single-dir workspace) or a workspace file — TOML
`dirs = ["a", "b"]`, relative entries resolved against the file's parent,
`.frances-workspace` extension by convention (not enforced). The path is
canonicalized and validated before the launcher detaches, so errors land on
the launching terminal.

**Every launch creates a fresh session.** There is no resume;
`SessionMeta.workspace_source` records the workspace's canonical identity
path so a future MRU/picker can enumerate sessions by workspace and reopen
them. The session's cwd is the workspace's primary dir (`dirs[0]`), not the
launching process's cwd.

Workspaces also carry a UUID identity: read from the workspace file's `id`
field, or generated in memory when opening a bare dir (or a file without an
`id`). `SessionMeta.workspace_id` snapshots it at session creation, and
saving the workspace (the `workspace::save` command) writes the same id into
the file — so sessions spawned before the save are already linked to it.

`editable_roots` currently derives from a marker walk on the primary dir;
switching it to the workspace's dirs verbatim is a known follow-up once
multi-dir workspaces are exercised.

## In-process runtime

There is no daemon. `frances` is a single process: the binary resolves the
workspace, creates the session, opens the
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
(chat history, scrollback, anchor, and entity tables) has **no
`session_id` columns** — sessions are isolated at the file level, not by
row. Don't reintroduce a global db at `state_root/frances.db`: turso uses
exclusive file locks and that caused cross-process contention back when
sessions were owned by separate daemons. `Session::database_path()` is
the source of truth.

## Entities

UI state that outlives a transcript position is an **entity**: a typed
envelope (`entity_id`, `kind` string, `lifecycle: Live | Settled`) plus
opaque-JSON facets — a small latest-wins **snapshot**, an optional
append-only **stream** (seq assigned by the hub; frontend subscriptions
get a gap-free catch-up + live tail), and **settle artifacts** (bounded
derived blobs written once at settle, point-read by tag). The
`EntityHub` (`frances-session/src/entities`) persists all three in the
per-session db and is a policy-free pipe: kind-specific decisions live
in producers (workflow JS via `frances:v1/entities`, or the runtime for
the workspace/session singletons) and in the frontend's per-kind
`{ Inline, Opened }` component pairs.

Lifecycle is the envelope's only non-identity field because core
machinery reads it: entities found Live at db open are **force-settled**
(their producer died with the previous process), and workflow
finish/dehydrate force-settles whatever the workflow left Live. The
transcript references entities via one-shot `SectionKind::EntityRef`
sections; the hub's attach snapshot is queued into the events channel
ahead of the scrollback replay, so snapshots always arrive before the
refs that need them.
