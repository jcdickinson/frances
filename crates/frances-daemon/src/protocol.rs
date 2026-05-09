use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::context::InvocationContext;
use crate::llm::Usage;

/// Identifies a single prompt-response cycle within a session. Server-assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PromptId(pub u64);

impl std::fmt::Display for PromptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Identifies a content block (user text, assistant text, etc.) within a
/// prompt-response cycle. Distinct from `PromptId` to prevent accidental
/// substitution when both are in scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlockId(pub u64);

impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Session identifier (currently a hex string from `generate_session_id`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// PID of the daemon process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DaemonPid(pub u32);

impl std::fmt::Display for DaemonPid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// Build-time protocol id derived from build-dir path + unix time + random bytes,
// SHA-256 hashed and truncated to 8 bytes interpreted as u64. Different builds
// produce different ids, so daemon and client must come from the same build.
include!(concat!(env!("OUT_DIR"), "/protocol_id.rs"));

#[tarpc::service]
pub trait Client {
    async fn attach(context: InvocationContext) -> AttachResponse;
    async fn detach();
    async fn prompt(prompt_id: PromptId, text: String) -> Result<(), String>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AttachResponse {
    Attached { session_id: SessionId },
    Busy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamFrame {
    BlockStart { id: BlockId, kind: BlockKind },
    BlockDelta { id: BlockId, text: String },
    BlockStop { id: BlockId },
    Usage(Usage),
    Done,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BlockKind {
    UserText,
    AssistantText,
    ToolUse { name: String },
    ToolResult { tool_use_id: String, is_error: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub session_id: SessionId,
    pub client_attached: bool,
    pub daemon_pid: DaemonPid,
    pub control_socket_path: PathBuf,
    pub client_socket_path: PathBuf,
    pub events_socket_path: PathBuf,
    pub protocol_version: u64,
}
