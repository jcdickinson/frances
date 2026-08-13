# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Process

Before starting planning, check `jj show --stat` and determine if the new work is completely unrelated. Query the user if so.

Upon completing work, `jj describe` the changes - including correcting the description if a new commit
wasn't created.

## Project

Frances is an agentic coding tool with a Tauri desktop app. The launcher opens a workspace (a directory, or a workspace file listing several dirs), creates a fresh session for it, opens the per-session turso database (the `turso` crate, successor to libsql — do not refer to it as libsql), and constructs an in-process `SessionRuntime`. A Svelte frontend renders the runtime's event stream.

**Frances has never shipped — there is no backward-compatibility burden.** No released version, no users with persisted state to preserve. DB schemas, on-disk serialization formats, wire shapes, and public APIs can change freely; do not write migrations, compatibility shims, or "old behaviour" fallbacks for the sake of existing data. When a refactor improves the type or format, just make the change.

## Workspace layout

The interesting components:

- **`frances`** — the Tauri binary, detached launcher, and event bridge.
- **`frontend`** — the Svelte + SCSS interface, developed and built with Deno.
- **`frances-session`** — session runtime: per-session DB handle, workflow selection, history, scrollback persistence, anchor store, llm session provider, and UI event channel.
- **`frances-workflow`** — JS-driven workflow runtime (rquickjs) that drives chat sessions and tool calls.
- **`frances-edit`** — anchor-based file edit engine. Filesystem-agnostic.
- **`frances-anchors`** — anchor word dictionary plus line hashing and word↔index encoding.

Workspace pins a single Rust toolchain in `rust-toolchain.toml` (1.95.0, edition 2024).

## Architecture docs

Read these before changing the relevant area:

- [`docs/arch/session-runtime.md`](docs/arch/session-runtime.md) — in-process runtime model, workspace/session layout, per-session database.
- [`docs/arch/edit-engine.md`](docs/arch/edit-engine.md) — how `frances-edit` and `frances-anchors` are wired into the binary.
- [`docs/arch/anchors.md`](docs/arch/anchors.md) — full anchor system design (line anchors, reconciliation, word pool, edit tool flow).

## Planning docs

In-progress plans live in `docs/plan/` — **untracked scratch** (it's in
`.gitignore`) that gets emptied as each item lands on `main`. When iterating on
a plan that already exists there, **edit the existing file in place** rather
than spawning new ones; only add or replace a file when the plan genuinely
changes shape. (Same scratch convention `docs/newui/` used.)

## Common commands

```bash
cargo build                       # build everything
cargo build -p frances            # just the binary (matches Nix flake)
cd frontend && deno task build    # build the Svelte frontend
cd frontend && deno task check    # type-check the frontend
deno task --config frontend/deno.json app # run with frontend HMR
cargo nextest                        # all tests
cargo nextest -p frances-edit        # one crate
cargo nextest -p frances-edit reconcile::tests::name_of_test  # one test
cargo fmt --all
cargo clippy --all-targets
cargo machete                     # find unused crate dependencies (provided by the devShell)
nix build                         # reproducible build via flake.nix
```

`cargo machete` is the unused-dependency check. Prefer it over rustc's
`unused_crate_dependencies` lint: that lint fires per compilation target, so
shared dev-deps and `path = "."` self-deps show up as false positives.
`cargo machete` reads each `Cargo.toml` against actual source references and
doesn't have that problem.

The user runs the dev shell via `nix develop` (provides toolchain + `rust-analyzer` + `jq` + `python3`).

`frances` CLI:

- `frances [path]` — open a directory or workspace file (defaults to `.`) in a fresh session and return immediately. Every launch is a new session.
- `frances --workflow <name>` — start the session with a specific workflow.
- `frances --foreground` — run the desktop app attached to the launcher process.
- `frances install [--local]` — write a starter config and install the `main` workflow.

## Code style and conventions

The user's global instructions (in `~/.claude/CLAUDE.md`) emphasise simplicity, deletion over accumulation, and direct/clear code over clever one-liners. They use `jj` (jujutsu) instead of `git` for version control — there's no staging area, the working copy is always part of a commit. Use `jj st`, `jj diff`, `jj describe`, `jj new`, `jj commit -m`. Use `jj git push/fetch` for remote operations.

For Rust documentation lookups, use `rsdoc` (the ferrisfetch MCP CLI) — `rsdoc add <crate>` then `rsdoc search` / `rsdoc get`. Don't curl docs.rs and don't dig in `~/.cargo`.

### Rust rules

- **Never `#[allow(...)]`, always `#[expect(...)]`.** `expect` fails the build if the lint stops firing, so dead suppressions don't accumulate. If you need to silence a lint, use `expect` with a reason.
- **Never suppress `clippy::too_many_arguments`.** It never holds up — "threading state by-ref" is not a justification, it's the smell. Fix the signature: bundle the related params into a struct (or pass the struct that already owns them by-ref). If the borrow signature gets longer, that's the cost of the function doing too much; reduce what it touches.
- **Make invalid states unrepresentable.** Push validity into the type system rather than runtime checks. If a field has only one legal value, it doesn't belong in the struct; if two fields are mutually exclusive, they belong in an enum.
- **Don't code like it's Python.** Reach for Rust's discriminated unions, newtypes, and trait bounds before stringly-typed shapes. Tagged enums beat `kind: &'static str` + optional fields:

  ```rust
  // BAD — invalid combinations are representable, "function" is enforced at runtime.
  pub struct ToolDef {
      #[serde(rename = "type")]
      pub kind: &'static str, // always "function" for now
      pub function: ToolFunction,
  }

  // GOOD — the tag and the payload are bound together by the type.
  #[serde(tag = "type", rename_all = "snake_case")]
  pub enum ToolDef {
      Function(ToolFunction),
  }
  ```

- **Lib crates MUST use thiserror.** Not anyhow, that is only suitable for bin crates.

## Conventions

## Unused

If things are kept around for functional reasons (e.g. temp dirs), don't `#[expect(unused..)] foo: ...`,
the correct thing to do is `_foo: ...`.

**Never suppress a dead-code warning with a "this will be used in the future" reason** — parity, symmetry, "write support is a follow-up", "useful for tracing later". What if it never is? The warning is the signal that the code has no purpose *right now*; silencing it removes the signal and the dead code accumulates. Delete it and let the warning stay hot. If the future use lands, add the field/fn back *then*, when it has a real reader. The only `#[expect(dead_code)]` that's legitimate is for something with a present-tense functional purpose that the compiler can't see — e.g. a field held purely for its `Drop` side effect (and only when `_foo:` can't express it, such as a positional tuple-variant field).

### Deps

We bring in deps using Deps traits that go into a `deps.rs` in their crate. For example:

```rs
// Generally always Send + Sync + Clone + 'static
pub trait FooDeps : Send + Sync + Clone + 'static {
    type Frobnicator : Frobnicator;

    fn fronbnicator(&self) -> Frobnicator;
}
```

Sometimes these traits will have DTOs that would unavoidably introduce crate cycles or
architectural hacks. That is what the `frances-models-*` crates are for. `frances-models-*`
MUST NOT have deps traits - those belong in the assosciated/logic crate. Traits (that do not use
deps traits themselves) can also be in the `frances-models-*` crate - e.g. `ChatSessionManager`,
this should be considered an extremely rare scenario.

## Crates

- `parking_lot` - avoid std sync primitives where possible.
- `dashmap` - avoid `Mutex<HashMap>` unless broad atomicity is needed for some reason.
- `anyhow` - generally exclusive to `frances` crate.
- `thiserror` - the mandatory mechanism for lib errors. Do not invent a shitty anyhow with this (e.g. `Msg(String)`). Errors don't have to be super precise, and don't have to carry the original error (`trace` it if is discarded), and obviously can still contain a message if they are more specific than `Msg`.

### Stringly-typed errors

Don't thread a bare `String` error (`Result<T, String>`, or an `Err(String)` arm) through Rust. "It only ends up as text in JS / a log / a tool result" is not a reason to demote early — that's what `Display` is for. Keep the typed error all the way to the boundary and let the sink call `.to_string()` / format it there. The typed error produces the exact same string, and until that final edge Rust can still match on the variant.

If a value is genuinely terminal — produced only to be handed to a sink and never inspected in Rust — name that with a newtype so the contract is visible and the type system stops it leaking back into control flow:

```rs
// Contract: only ever rendered as a JS exception message. Never matched in Rust.
// Built by Display-ing a typed error at the boundary, not by stringifying early.
pub struct JsError(pub String);
```
