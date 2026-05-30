use std::path::PathBuf;

use thiserror::Error;

use crate::storage::WorkflowDbError;

#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("split workflow args: {0}")]
    SplitArgs(#[from] shell_words::ParseError),

    #[error("read workflow source: {0}")]
    ReadSource(#[source] std::io::Error),

    #[error("read workflow migration {path}: {source}")]
    ReadMigration {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("parse workflow source: {0}")]
    Parse(#[from] deno_ast::ParseDiagnostic),

    #[error("transpile workflow source: {0}")]
    Transpile(#[from] deno_ast::TranspileError),

    #[error("could not derive a file:// specifier for {0}")]
    TranspileSpecifier(PathBuf),

    #[error("script: {0}")]
    Script(#[from] rquickjs::Error),

    #[error("script ({context}): {detail}")]
    ScriptCaught {
        context: &'static str,
        detail: String,
    },

    #[error("build workflow JS thread runtime: {0}")]
    JsThreadRuntime(#[source] std::io::Error),

    #[error("spawn workflow JS thread: {0}")]
    JsThreadSpawn(#[source] std::io::Error),

    #[error("workflow JS thread unavailable")]
    JsThreadGone,

    #[error(transparent)]
    Storage(#[from] WorkflowDbError),
}
