use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::context::InvocationContext;
use crate::llm::Usage;

pub type PromptId = u64;

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
    Attached { session_id: String },
    Busy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamFrame {
    BlockStart { id: u64, kind: BlockKind },
    BlockDelta { id: u64, text: String },
    BlockStop { id: u64 },
    Usage(Usage),
    Done,
    Error(String),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum BlockKind {
    UserText,
    AssistantText,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub session_id: String,
    pub client_attached: bool,
    pub daemon_pid: u32,
    pub control_socket_path: PathBuf,
    pub client_socket_path: PathBuf,
    pub events_socket_path: PathBuf,
    pub protocol_version: u64,
}
