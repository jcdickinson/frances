use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Content, Feed};

pub const PROTOCOL_VERSION: u32 = 3;

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
    FsFindOrGrep {
        options: FileSearchOptions,
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
    FileSearch {
        results: Feed<FileSearchEvent>,
    },
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

#[derive(Debug, Serialize, Deserialize)]
pub struct FileSearchOptions {
    pub cwd: Option<PathBuf>,
    pub root: Option<PathBuf>,
    pub query: FileSearchQuery,
    pub exclude: Vec<String>,
    pub ignore: bool,
    pub hidden: bool,
    pub depth: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "query", rename_all = "snake_case")]
pub enum FileSearchQuery {
    All,
    Paths {
        patterns: FileSearchPatterns,
    },
    Search {
        regex: String,
        paths: Vec<String>,
        matches: FileSearchMatchMode,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileSearchMatchMode {
    Count,
    Content,
}

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct FileSearchPatterns(Vec<String>);

impl FileSearchPatterns {
    pub fn new(patterns: Vec<String>) -> Option<Self> {
        if patterns.is_empty() {
            None
        } else {
            Some(Self(patterns))
        }
    }

    pub fn into_vec(self) -> Vec<String> {
        self.0
    }
}

impl<'de> Deserialize<'de> for FileSearchPatterns {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let patterns = Vec::<String>::deserialize(deserializer)?;
        Self::new(patterns)
            .ok_or_else(|| serde::de::Error::custom("file search patterns cannot be empty"))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum FileSearchEvent {
    Listed {
        file: FileSearchFile,
        binary: bool,
    },
    Counted {
        file: FileSearchFile,
    },
    Matched {
        file: FileSearchFile,
        matched: FileSearchMatch,
    },
    Done {
        truncated_at: Option<NonZeroUsize>,
    },
    Error {
        error: ResponseError,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSearchFile {
    pub path: PathBuf,
    pub size: u64,
    pub mtime_ns: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileSearchMatch {
    pub line: NonZeroU64,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_bytes: Option<NonZeroUsize>,
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
