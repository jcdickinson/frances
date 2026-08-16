use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Content, Feed};

pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Filesystem,
    Shell,
}

pub type ShellId = u64;

#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub id: u64,
    #[serde(flatten)]
    pub kind: RequestKind,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum RequestKind {
    Hello,
    FsRead {
        path: PathBuf,
    },
    FsWrite {
        path: PathBuf,
        content: Content,
        mode: FsWriteMode,
    },
    FsMetadata {
        path: PathBuf,
    },
    FsCreateDirAll {
        path: PathBuf,
    },
    FsCanonicalize {
        path: PathBuf,
    },
    ShellOpen {
        options: ShellOptions,
    },
    ShellRun {
        shell: ShellId,
        script: String,
        stdin: Option<Content>,
        persist: Vec<String>,
    },
    ShellWaitQuiet {
        shell: ShellId,
        quiet_ms: u64,
    },
    ShellKill {
        shell: ShellId,
    },
    ShellSetVar {
        shell: ShellId,
        name: String,
        value: Content,
    },
    ShellGetVar {
        shell: ShellId,
        name: String,
    },
    ShellClose {
        shell: ShellId,
    },
    Cancel {
        request: u64,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    pub id: u64,
    #[serde(flatten)]
    pub result: Result<ResponseKind, ResponseError>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", content = "value", rename_all = "snake_case")]
pub enum ResponseKind {
    Hello(Hello),
    Content(Content),
    Metadata(FsMetadata),
    Path(PathBuf),
    ShellOpened {
        shell: ShellId,
        output: Feed<ShellOutput>,
    },
    ShellWaitQuiet(ShellWaitQuiet),
    Unit,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ShellOptions {
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub init_script: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "output", rename_all = "snake_case")]
pub enum ShellOutput {
    Output { content: Content },
    Exit { exit_code: i32 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellWaitQuiet {
    Quiet,
    Exit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FsMetadata {
    pub mtime_ns: i64,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsWriteMode {
    Overwrite,
    CreateNew,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    Io,
    AlreadyExists,
    UnsupportedVersion,
    Internal,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResponseError {
    pub error: ErrorBody,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

impl ResponseError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code,
                message: message.into(),
            },
        }
    }
}
