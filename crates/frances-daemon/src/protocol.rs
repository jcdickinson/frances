use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use uuid::Uuid;

use crate::context::InvocationContext;
use crate::llm::Usage;

pub use frances_workflow::permission::{PermissionId, PermissionRequest, PermissionResponseWire};

/// Identifies a content block (user text, assistant text, etc.) within a
/// prompt-response cycle.
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
    async fn prompt(text: String) -> Result<(), String>;
    /// Submit the user's response to an outstanding `Permission`
    /// request previously emitted as a `StreamFrame::Permission`.
    async fn respond_permission(
        id: PermissionId,
        response: PermissionResponseWire,
    ) -> Result<(), String>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AttachResponse {
    Attached { session_id: SessionId },
    Busy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamFrame {
    /// Self-describing block content. The first delta with a
    /// previously-unseen `id` implicitly opens a new block of `kind`;
    /// subsequent deltas with the same id append to that block's
    /// text. There is no separate "block start" frame — that way a
    /// client that connects mid-block (or reconnects after the events
    /// socket died) can construct the block from the very next delta
    /// without needing to have seen the start.
    ///
    /// `text` is `None` when the frame carries no body delta this
    /// round — either an opener for a block that was pushed without
    /// initial content (the workflow will write to it later) or an
    /// in-place metadata transition (kind-only update). The client
    /// tracks the id but skips measure/render until the first
    /// `Some(_)` text arrives.
    BlockDelta {
        id: BlockId,
        kind: BlockKind,
        text: Option<String>,
    },
    BlockStop {
        id: BlockId,
    },
    /// Replay-only sibling of `BlockStop`: the block was in flight when
    /// its workflow was dehydrated, so it never received a clean stop.
    /// The daemon emits this in place of `BlockStop` from
    /// [`crate::scrollback::replay_to_stream`] (or
    /// [`crate::scrollback::replay_frames`]) for rows whose `truncated`
    /// column is set. The TUI renders the block with a visible
    /// "(truncated)" indicator.
    BlockTruncated {
        id: BlockId,
    },
    Usage(Usage),
    Done,
    Error(String),
    /// Server is asking the user for permission; client responds via
    /// the `Client::respond_permission` RPC.
    Permission(PermissionRequest),
    /// Replay opener for the currently-active workflow's scrollback.
    /// The TUI clears its in-memory scrollback container, enters replay
    /// mode, and routes the subsequent block / error frames straight
    /// into the alt-screen inspector's committed deque. Live drawing
    /// to the live viewport is skipped until
    /// [`StreamFrame::ScrollbackReplayEnd`].
    ScrollbackReset {
        instance_id: Uuid,
    },
    /// Replay closer. The TUI returns to normal live-mode handling.
    ScrollbackReplayEnd,
}

/// Distinguishing tag for a block. Fields are [`Arc<str>`] so that
/// the enum is cheap to clone — every [`StreamFrame::BlockDelta`] now
/// carries a `BlockKind`, so cloning is on the hot path. `Arc<str>`
/// serializes the same way `String` does on the wire (writes the
/// string body, allocates a fresh `Arc` on deserialize) and compares
/// via the underlying `str`, so callers see no behavioural change
/// beyond the cheap clone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockKind {
    /// A free-form text block. `sender` labels the speaker; the TUI
    /// renders it as a prefix when present, and renders nothing in
    /// front of the body when `None`. Workflows pick the sender via
    /// `new MarkdownFrame({ content, sender })` — there is no
    /// host-side meaning beyond the label.
    Text { sender: Option<Arc<str>> },
    /// `detail` is an optional human-readable suffix sourced from the
    /// tool's `describe(call)` method (e.g. the file path + ranges for
    /// `file_read`). The TUI renders it after `name` in a dim style.
    ToolUse {
        name: Arc<str>,
        detail: Option<Arc<str>>,
    },
    /// Streaming output from a shell command. The body carries the
    /// accumulated stdout; `cmd` is the bash source that produced it,
    /// pinned separately so the TUI can render it as a header even when
    /// the body is truncated. `state` advances from `Running` to
    /// `Success`/`Exit(N)` as the command completes, with the TUI
    /// re-rendering on each transition.
    ShellOutput { state: ShellState, cmd: Arc<str> },
}

/// Terminal-status enum for [`BlockKind::ShellOutput`]. Carried on
/// every `BlockDelta` so an in-place transition is just a no-text
/// delta with the new state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellState {
    /// Command is still in flight.
    Running,
    /// Command exited 0.
    Success,
    /// Command exited with a non-zero code (or was killed before exit).
    Exit(i32),
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
