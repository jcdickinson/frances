use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use tracing::trace;

use crate::workspace::Workspace;

#[derive(Debug, Clone)]
pub struct ProcessContext {
    pub cwd: Option<PathBuf>,
    pub env: Arc<HashMap<OsString, OsString>>,
}

#[derive(Debug, Clone)]
pub struct InvocationContext {
    pub workspace: Workspace,
    pub process: ProcessContext,
}

impl InvocationContext {
    /// Snapshot the launch context. `cwd` comes from the workspace's
    /// primary dir, not the launching process — the session's working
    /// directory must not depend on where it was relaunched from. Env
    /// still snapshots the real process environment (secrets arrive
    /// that way).
    pub fn capture(workspace: Workspace) -> Self {
        let context = Self {
            process: ProcessContext {
                cwd: Some(workspace.primary_dir().to_path_buf()),
                env: Arc::new(env::vars_os().collect()),
            },
            workspace,
        };

        trace!(
            workspace = %context.workspace.source.identity_path().display(),
            env_vars = context.process.env.len(),
            "captured invocation context"
        );

        context
    }
}
