use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use tracing::trace;

use crate::tty::TtyKey;

#[derive(Debug, Clone)]
pub struct ProcessContext {
    pub cwd: Option<PathBuf>,
    /// Shared so per-completion reads (`current_env`) clone an `Arc`, not the
    /// whole several-hundred-entry map. The context is replaced wholesale via
    /// `update_invocation`, never mutated in place, so the sharing is safe.
    pub env: Arc<HashMap<OsString, OsString>>,
}

#[derive(Debug, Clone)]
pub struct InvocationContext {
    pub tty_key: Option<TtyKey>,
    pub process: ProcessContext,
}

impl InvocationContext {
    pub fn capture(tty_key: Option<TtyKey>) -> Self {
        let context = Self {
            tty_key,
            process: ProcessContext {
                cwd: env::current_dir().ok(),
                env: Arc::new(env::vars_os().collect()),
            },
        };

        trace!(
            has_tty_key = context.tty_key.is_some(),
            env_vars = context.process.env.len(),
            has_cwd = context.process.cwd.is_some(),
            "captured invocation context"
        );

        context
    }
}
