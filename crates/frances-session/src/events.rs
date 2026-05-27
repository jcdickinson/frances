//! Stream-event surface shared by producers (workflows, scrollback
//! replay) and the TUI consumer.
//!
//! There is no inter-process boundary — these types travel through an
//! in-process `tokio::sync::mpsc` from the session runtime to the TUI.
//! `Serialize` / `Deserialize` remain implemented because scrollback
//! persists block payloads to the turso DB as bincode rows, not because
//! the events themselves cross the wire.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use uuid::Uuid;

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
    /// The session runtime emits this in place of `BlockStop` from
    /// [`crate::scrollback::replay_to_channel`] (or
    /// [`crate::scrollback::replay_frames`]) for rows whose `truncated`
    /// column is set. The TUI renders the block with a visible
    /// "(truncated)" indicator.
    BlockTruncated {
        id: BlockId,
    },
    Usage(Usage),
    /// Workflow-set busy-indicator text. `Some(text)` → the TUI footer
    /// shows the text with a spinner; `None` → hidden. Driven by
    /// `setStatus` in the workflow; not persisted, dropped during replay.
    Status(Option<String>),
    Done,
    Error(String),
    /// Runtime is asking the user for permission; client responds via
    /// [`crate::runtime::SessionRuntime::respond_permission`].
    Permission(PermissionRequest),
    /// A frame of the scrollback-replay sub-protocol. A burst is
    /// bracketed by [`ScrollbackFrame::Reset`] / [`ScrollbackFrame::End`]
    /// and carries its own block frames — a closed, bounded set distinct
    /// from the live variants above, so the TUI's replay handler never
    /// has to reason about live-only frames.
    Scrollback(ScrollbackFrame),
}

/// The scrollback-replay sub-protocol: exactly the frames a replay burst
/// emits, in their own type so consumers match a small, real set with no
/// defensive arms. Produced by [`crate::scrollback::replay_to_channel`] /
/// [`crate::scrollback::replay_frames`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScrollbackFrame {
    /// Burst opener: the TUI clears its in-memory scrollback container
    /// and replays `instance_id`'s persisted history into the alt-screen
    /// inspector's committed deque.
    Reset { instance_id: Uuid },
    /// A persisted block's content (same shape as a live `BlockDelta`).
    Block {
        id: BlockId,
        kind: BlockKind,
        text: Option<String>,
    },
    /// Clean end of a persisted block.
    BlockStop { id: BlockId },
    /// Truncated end — the block was in-flight when its workflow was
    /// dehydrated and never received a clean stop.
    BlockTruncated { id: BlockId },
    /// A persisted error row.
    Error(String),
    /// Burst closer: the TUI returns to live-mode handling.
    End,
}

/// Distinguishing tag for a block. Fields are [`Arc<str>`] so that
/// the enum is cheap to clone — every [`StreamFrame::BlockDelta`] now
/// carries a `BlockKind`, so cloning is on the hot path.
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
    /// A unified diff block.
    Diff { lines: Vec<DiffLine> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLine {
    Context { text: Arc<str>, line: u32 },
    Added(Arc<str>),
    Removed(Arc<str>),
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
