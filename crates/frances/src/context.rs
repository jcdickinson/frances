use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::trace;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessContext {
    pub cwd: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationContext {
    pub tty_key: Option<String>,
    pub process: ProcessContext,
}

impl InvocationContext {
    pub fn capture(tty_key: Option<String>) -> Self {
        let context = Self {
            tty_key,
            process: ProcessContext {
                cwd: env::current_dir().ok(),
                env: env::vars_os().collect(),
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
