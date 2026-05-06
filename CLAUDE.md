# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Frances is an agentic coding tool. The `frances` binary is a TUI front-end that talks to a per-TTY background daemon over Unix sockets; the daemon owns the session, persists history to a per-session turso (libsql) database, and streams LLM completions via OpenRouter.

## Workspace layout

Three crates under `crates/`:

- **`frances`** — the binary. Daemon (`src/daemon/`), TUI (`src/tui/`, `src/ui.rs`), session/path management (`src/session.rs`), turso wrapper (`src/store.rs`), LLM client (`src/llm.rs`), edit-tool plumbing (`src/edit_session.rs`, `src/anchor_store.rs`).
- **`frances-edit`** — anchor-based file edit engine. Filesystem-agnostic.
- **`frances-anchors`** — anchor word dictionary plus line hashing and word↔index encoding.

Workspace pins a single Rust toolchain in `rust-toolchain.toml` (1.95.0, edition 2024).

## Architecture docs

Read these before changing the relevant area:

- [`docs/arch/daemon.md`](docs/arch/daemon.md) — daemon/session/TTY model, socket layout, protocol versioning, per-session database.
- [`docs/arch/edit-engine.md`](docs/arch/edit-engine.md) — how `frances-edit` and `frances-anchors` are wired into the binary.
- [`docs/arch/anchors.md`](docs/arch/anchors.md) — full anchor system design (line anchors, reconciliation, word pool, edit tool flow).

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

- `frances` — attach to (or create) the daemon for the current TTY and open the TUI.
- `frances new` — stop the daemon for this TTY and unlink it, so the next run starts a fresh session.
- `frances daemon status` / `frances daemon stop` — inspect/stop the daemon for this TTY.
- `frances --daemon <session_id>` — internal flag the binary uses to re-exec itself as the daemon; don't invoke directly.

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
