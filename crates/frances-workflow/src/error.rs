use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("split workflow args: {0}")]
    SplitArgs(#[from] shell_words::ParseError),

    #[error("read workflow source: {0}")]
    ReadSource(#[source] std::io::Error),

    #[error("transpile workflow source: {0}")]
    Transpile(String),

    #[error("could not derive a file:// specifier for {0}")]
    TranspileSpecifier(PathBuf),

    #[error("script engine: {0}")]
    Script(String),
}
