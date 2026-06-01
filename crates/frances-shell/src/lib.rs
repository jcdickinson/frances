//! A long-lived bash subprocess for LLM-driven shells.
//!
//! Each [`Shell`] owns one bash process. Commands submitted via
//! [`Shell::run`] are dot-sourced into that bash, so env vars, `cd`,
//! shell functions, aliases, sourced scripts, and `shopt` flags persist
//! across calls. Callers can write *any* bash code they would write at a
//! real prompt — pipelines, multi-line `if`/`for`/`while`, subshells,
//! heredocs, function definitions, `set -e`, `trap`, etc. — verbatim. No
//! `bash -c '...'` wrapping, no escaping, no single-lining required.
//!
//! Output framing uses a per-shell random sentinel templated literally
//! into the wrapper bytes, so user code can't shadow / `unset` /
//! `readonly` it to break the protocol. Stderr is merged into stdout
//! inside bash via `exec 2>&1`, so callers see one ordered stream.
//!
//! [`Shell::run`] returns [`RunOutcome::Done`] when the command finishes,
//! [`RunOutcome::Quiet`] when output goes silent (or a max wall-clock
//! ceiling trips), and [`RunOutcome::Dead`] when bash itself exits.
//! [`Shell::keep_waiting`] continues an in-flight command;
//! [`Shell::interrupt`] / [`Shell::kill_running`] signal the running
//! command's PID directly without taking down bash.
//!
//! Pipes-only — no PTY in v1. Apps that hard-require a TTY (`vim`,
//! `top`, `psql` without `-c`) are unsupported here; their
//! non-interactive equivalents (`psql -c "SELECT 1"`, `ssh host cmd`)
//! work fine.
//!
//! Each [`Shell`] is fully self-contained: its own bash, nonce, tmpdir,
//! and reader task. Multiple shells can run in parallel without
//! interfering — signals target a specific bash's child PID, never a
//! process group.

mod child;
mod error;
mod proto;
mod reader;
mod shell;

pub use error::{ShellError, ShellResult};
pub use reader::ReadEvent;
pub use shell::{DEFAULT_QUIET, QuietReason, RunOutcome, Shell, ShellOptions, WaitOpts};
