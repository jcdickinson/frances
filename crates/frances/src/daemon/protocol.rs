use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::context::InvocationContext;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlRequest {
    Ping,
    Status,
    Stop { delete_state: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlResponse {
    Pong,
    Status(DaemonStatus),
    Stopping,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientRequest {
    Attach { context: InvocationContext },
    Detach,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientResponse {
    Attached { session_id: String },
    Busy,
    Detached,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub session_id: String,
    pub client_attached: bool,
    pub daemon_pid: u32,
    pub control_socket_path: PathBuf,
    pub client_socket_path: PathBuf,
    pub protocol_version: u32,
}
