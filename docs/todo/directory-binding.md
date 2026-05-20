# Directory binding

Bind a session to a working directory so that bare `frances` resumes that session in a fresh TTY, instead of starting a new session. Survives reboots and TTY changes; only `frances new` opts out.

## Motivation

Today, bare `frances` resolves the current TTY to a session via `runtime_root/tty-links/<tty_key>` (see `docs/arch/daemon.md`). TTY links are ephemeral, so moving terminals or rebooting drops the link and the next `frances` starts a fresh session even though the project hasn't moved. Directory binding lets a project "remember" its session across TTYs by indexing on the path the user `cd`'d to.

## User-facing behavior

- `frances bind` — bind the current directory to a session. If the current TTY already has a session, bind cwd → that session. Otherwise create a new session, attach, and bind cwd → it. Refuses to overwrite an existing exact binding without `--force`.
- `frances bind list` — list all bindings (dir → session id).
- `frances bind rm` — remove the binding for the current directory (session itself untouched). Takes an optional path argument to remove a different binding.
- `frances` (no subcommand):
  1. If the current TTY already has a session → attach as today.
  2. Else look up cwd in the bindings store using ancestor search: walk up the literal cwd, take the deepest match.
     - Match exists and the bound session is alive on disk → spawn its daemon if needed, link the current TTY to it, attach.
     - Match exists but the bound session_dir is gone → log, clear the stale row, fall through to fresh-session path.
     - Bound session is locked by another TTY (daemon returns `AttachResponse::Busy`) → print a warning naming the session id and exit non-zero. Do **not** silently spawn a different session.
  3. No match → today's behavior: fresh session for this TTY.
- `frances new` — unchanged. Ignores the dir binding entirely; always spins a fresh session for the current TTY. Use this to deliberately bypass a binding.

## Paths

Paths go in as-typed. No `canonicalize`, no symlink resolution, no `..` normalization beyond what the OS already gave us. The user binds the path they see; two symlinked routes to the same project get two independent bindings, which is the intended behavior.

This also makes ancestor search a literal string-prefix walk over path components, with no surprises from filesystem state.

## Storage

Bindings live in an **app-wide** store (not per-session). Schema sketch:

| column     | type    | notes                                 |
|------------|---------|---------------------------------------|
| `path`     | TEXT PK | as-typed absolute path                |
| `session`  | TEXT    | session id (matches `<state_root>/sessions/<id>/`) |
| `created`  | INTEGER | unix seconds, for `bind list`         |

Lookup for `frances` startup: `SELECT session FROM bindings WHERE ? LIKE path || '/%' OR ? = path ORDER BY length(path) DESC LIMIT 1`, or the in-Rust equivalent walking the path upward.

This store does **not** belong in any session's `frances.db` — those are file-locked by their daemons and partitioned by design (see `docs/arch/daemon.md`'s warning about reintroducing a global db). The bindings store is touched only by the short-lived `frances` client process at startup/`bind`/`bind rm`, so there's no daemon contention. It also has no `session_id` per row in any other table — it's the whole table.

The store doesn't exist yet. When it lands, this is its first user; future app-wide metadata (e.g. global preferences) can share it.

## Resolution & lock detection

Add `bind` lookup/insert/delete to the bindings store and wire the lookup into the bare-`frances` arm in `crates/frances/src/main.rs:98-121`, before today's `resolve_or_create_for_tty` call. Sketch:

```text
None => {
    if let Some(session) = paths.resolve_tty_link(&tty_key)? {
        // existing behavior: TTY already linked, use it
    } else if let Some(bound) = bindings::find_for(&cwd)? {
        let session = Session::open(&paths, &bound)?;
        spawn::ensure_daemon(&session).await?;
        match client::attach(&session, invocation).await? {
            Attached { .. } => paths.link_tty(&tty_key, &session)?,  // adopt
            Busy => { eprintln!("frances: session {} is bound here but attached on another TTY; `frances new` to override", session.id); return Ok(()); }
        }
    } else {
        // existing fresh-session path
    }
}
```

Lock detection: pre-rip this rode on the daemon's `AttachResponse::Busy` machinery. With the in-process model there's no "second client" to detect — a second `frances` invocation against the same session_dir is just another process opening the same turso DB. Needs a separate lockfile (e.g. `runtime_dir/lock` + `flock`) before this design can ship.

Identifying *which* TTY holds the lock is a nice-to-have: scan `runtime_root/tty-links/` for any link pointing at this session_dir whose key differs from ours. Not required for v1.

## Out of scope

- Multi-user bindings (the store is per-`state_root`, i.e. per-user).
- `.frances` project marker files for discovery — ancestor walk over the bindings table covers the same case without adding files to projects.
- Auto-binding without an explicit `frances bind`.
- Bindings keyed on git repo root or VCS metadata.
