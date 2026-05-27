# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Process

Before starting planning, check `jj show --stat` and determine if the new work is completely unrelated. Query the user if so.

Upon completing work, `jj describe` the changes - including correcting the description if a new commit
wasn't created.

## Project

Frances is an agentic coding tool. The `frances` binary is a single-process TUI: it identifies the controlling TTY, resolves (or creates) a per-TTY session, opens a per-session turso (libsql) database, constructs an in-process `SessionRuntime`, and runs the TUI directly against it. LLM completions stream via OpenRouter.

## Workspace layout

The interesting crates under `crates/`:

- **`frances`** — the binary. TUI (`src/tui/`, `src/ui.rs`), TTY identification (`src/tty.rs`), `main.rs` wires the runtime to the TUI.
- **`frances-session`** — session runtime: per-session DB handle, workflow stack, history, scrollback persistence, anchor store, llm session provider, events channel into the TUI.
- **`frances-workflow`** — JS-driven workflow runtime (rquickjs) that drives chat sessions and tool calls.
- **`frances-edit`** — anchor-based file edit engine. Filesystem-agnostic.
- **`frances-anchors`** — anchor word dictionary plus line hashing and word↔index encoding.

Workspace pins a single Rust toolchain in `rust-toolchain.toml` (1.95.0, edition 2024).

## Architecture docs

Read these before changing the relevant area:

- [`docs/arch/session-runtime.md`](docs/arch/session-runtime.md) — in-process runtime model, session/TTY layout, per-session database.
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
cargo nextest                        # all tests
cargo nextest -p frances-edit        # one crate
cargo nextest -p frances-edit reconcile::tests::name_of_test  # one test
cargo fmt --all
cargo clippy --all-targets
nix build                         # reproducible build via flake.nix
```

The user runs the dev shell via `nix develop` (provides toolchain + `rust-analyzer` + `jq` + `python3`).

`frances` subcommands:

- `frances` — open the TUI against the current TTY's session, creating one if none is linked.
- `frances new` — unlink the current TTY's session so the next run creates a fresh session. The old session's state on disk is left intact.

## Code style and conventions

The user's global instructions (in `~/.claude/CLAUDE.md`) emphasise simplicity, deletion over accumulation, and direct/clear code over clever one-liners. They use `jj` (jujutsu) instead of `git` for version control — there's no staging area, the working copy is always part of a commit. Use `jj st`, `jj diff`, `jj describe`, `jj new`, `jj commit -m`. Use `jj git push/fetch` for remote operations.

For Rust documentation lookups, use `rsdoc` (the ferrisfetch MCP CLI) — `rsdoc add <crate>` then `rsdoc search` / `rsdoc get`. Don't curl docs.rs and don't dig in `~/.cargo`.

### Rust rules

- **Never `#[allow(...)]`, always `#[expect(...)]`.** `expect` fails the build if the lint stops firing, so dead suppressions don't accumulate. If you need to silence a lint, use `expect` with a reason.
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
