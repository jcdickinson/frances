//! Stream-event surface shared by producers (workflows, scrollback
//! replay) and the TUI consumer.
//!
//! There is no inter-process boundary — these types travel through an
//! in-process `tokio::sync::mpsc` from the session runtime to the TUI.
//! Do not call this a "wire" or "protocol" in comments or variable
//! names; it's a channel of Rust enums. The term "wire" is reserved for
//! the LLM provider's HTTP boundary (see `frances-llm::Provider::kind`).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use uuid::Uuid;

pub use frances_workflow::{Source, SurfaceCmd};

use crate::llm::Usage;

pub use frances_workflow::permission::{
    PermissionRequest, PermissionResponse, PermissionResponseWire,
};

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

#[derive(Debug)]
pub enum StreamFrame {
    /// Self-describing block content. The first delta with a
    /// previously-unseen `id` implicitly opens a new block of `kind`;
    /// subsequent deltas with the same id append to that block's
    /// text. There is no separate "block start" frame — every delta is
    /// self-describing, so the consumer can construct the block from any
    /// delta without having seen the start.
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
    /// [`crate::scrollback::replay_to_channel`] for rows whose `truncated`
    /// column is set. The TUI renders the block with a visible
    /// "(truncated)" indicator.
    BlockTruncated {
        id: BlockId,
    },
    Usage(Usage),
    /// Workflow-declared chrome (the footer busy indicator today).
    /// `SetFooter`/`ClearFooter`. Driven by `setStatus` in the workflow;
    /// not persisted, dropped during replay.
    Surface(SurfaceCmd),
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
/// defensive arms. Produced by [`crate::scrollback::replay_to_channel`].
#[derive(Debug, Clone)]
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
    /// A free-form text block. `source` names the speaker; the TUI maps
    /// it to a single-grapheme sigil (`User` → `>`, `Assistant` → `◆`,
    /// `Internal` → no prefix). Workflows pick it via `new
    /// MarkdownSection({ source })`; `Internal` is the default when the
    /// workflow omits the field (chrome, JSON tag bodies, greetings).
    Text { source: Source },
    /// `detail` is an optional human-readable suffix sourced from the
    /// tool's `describe(call)` method (e.g. the file path + ranges for
    /// `file_read`). The TUI renders it after `name` in a dim style.
    ToolUse {
        name: Arc<str>,
        detail: Option<Arc<str>>,
    },
    /// A tailed streaming-output block — the TUI renders the last N
    /// lines of `body` with a status-coloured `[label]` header and an
    /// `… [N earlier lines]` collapse marker. Used for shell output
    /// (header = shell state + command line) and for model reasoning
    /// (header = reasoning state). The body advances by delta; the
    /// `header` field advances by mid-stream `kind` re-emits.
    Tailed { header: TailedHeader },
    /// A unified diff block.
    Diff { lines: Vec<DiffLine> },
}

/// Header content for a [`BlockKind::Tailed`] block. Each variant
/// determines the header label, its colour tone, and any pinned
/// metadata (the shell command line, the reasoning state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TailedHeader {
    /// Shell command output. `cmd` is the bash source pinned to the
    /// header so it survives body truncation. `state` advances from
    /// `Running` to `Success`/`Exit(N)` on completion.
    Shell { state: ShellState, cmd: Arc<str> },
    /// Streaming model reasoning. `state` advances from `Streaming`
    /// to `Done` when the channel closes.
    Reasoning { state: ReasoningState },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLine {
    Context { text: Arc<str>, line: u32 },
    Added(Arc<str>),
    Removed(Arc<str>),
}

/// Terminal-status enum for [`TailedHeader::Shell`]. Carried on
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

/// Terminal-status enum for [`TailedHeader::Reasoning`]. Same
/// re-emit-on-transition pattern as [`ShellState`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningState {
    /// Model is still emitting reasoning content.
    Streaming,
    /// Reasoning channel has closed; no further body deltas expected.
    Done,
}
