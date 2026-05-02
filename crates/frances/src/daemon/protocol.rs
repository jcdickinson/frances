use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::context::InvocationContext;
use crate::llm::Usage;

pub type PromptId = u64;

#[tarpc::service]
pub trait Control {
    async fn ping();
    async fn status() -> DaemonStatus;
    async fn stop(delete_state: bool);
}

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
    Text(String),
    Usage(Usage),
    Done,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub session_id: String,
    pub client_attached: bool,
    pub daemon_pid: u32,
    pub control_socket_path: PathBuf,
    pub client_socket_path: PathBuf,
    pub events_socket_path: PathBuf,
    pub protocol_version: u32,
}
