use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Content, Feed};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Filesystem,
    Shell,
}

pub type ShellId = u64;
pub type ShellOperationId = u64;

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
        commands: Feed<ShellCommand>,
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
        events: Feed<ShellEvent>,
    },
    Unit,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ShellOptions {
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub init_script: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ShellCommand {
    Run {
        operation: ShellOperationId,
        script: String,
        stdin: Option<Content>,
        persist: Vec<String>,
        wait: ShellWait,
    },
    KeepWaiting {
        operation: ShellOperationId,
        wait: ShellWait,
    },
    Kill {
        operation: ShellOperationId,
    },
    SetVar {
        operation: ShellOperationId,
        name: String,
        value: Content,
    },
    GetVar {
        operation: ShellOperationId,
        name: String,
    },
    Close,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ShellWait {
    pub quiet_ms: Option<u64>,
    pub max_ms: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ShellEvent {
    pub operation: ShellOperationId,
    #[serde(flatten)]
    pub kind: ShellEventKind,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ShellEventKind {
    Output { content: Content },
    Done { exit_code: i32 },
    Quiet { reason: ShellQuietReason },
    Dead,
    Ack,
    Value { content: Content },
    Error { message: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellQuietReason {
    NoOutput,
    MaxElapsed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FsMetadata {
    pub mtime_ns: i64,
    pub size: u64,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    Io,
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
