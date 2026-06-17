//! Quasi-persistent bash execution for LLM-driven shell tools.
//!
//! A [`Shell`] stores Frances-managed shell state, not a long-lived bash
//! process. Each [`Shell::run`] writes the user script to a temp file, wraps it
//! with a small Frances prelude/teardown script, and spawns one bash process for
//! that invocation. The wrapper restores the stored cwd and exported env, runs
//! the user script, captures teardown state into temp files, then emits a
//! nonce-framed sentinel carrying the saved user exit code.
//!
//! State that survives between invocations is intentionally narrow: cwd always
//! persists, and exported environment variables persist only when requested for
//! that run via [`RunOpts::persist`]. Shell functions, aliases, ordinary shell
//! variables, traps, and sourced scripts are process-local and disappear when
//! the invocation exits.
//!
//! [`Shell::run`] returns [`RunOutcome::Done`] when the sentinel arrives,
//! [`RunOutcome::Quiet`] when output goes silent (or a max wall-clock ceiling
//! trips), and [`RunOutcome::Dead`] when the invocation exits before framing a
//! result. [`Shell::keep_waiting`] continues an in-flight invocation;
//! [`Shell::interrupt`] / [`Shell::kill_running`] signal its process group.
//!
//! Pipes-only — no PTY in v1. Apps that hard-require a TTY (`vim`, `top`,
//! `psql` without `-c`) are unsupported here; their non-interactive equivalents
//! (`psql -c "SELECT 1"`, `ssh host cmd`) work fine.

mod child;
mod error;
mod proto;
mod reader;
mod shell;

pub use error::{ShellError, ShellResult};
pub use reader::ReadEvent;
pub use shell::{
    DEFAULT_QUIET, QuietReason, RunOpts, RunOutcome, Shell, ShellOptions, ShellSnapshot, WaitOpts,
};
