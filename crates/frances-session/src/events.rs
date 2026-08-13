//! Stream-event surface shared by producers (workflows, scrollback
//! replay) and the UI consumer.
//!
//! There is no inter-process boundary — these types travel through an
//! in-process `tokio::sync::mpsc` from the session runtime to the UI.
//! Do not call this a "wire" or "protocol" in comments or variable
//! names; it's a channel of Rust enums. The term "wire" is reserved for
//! the LLM provider's HTTP boundary (see `frances-llm::Provider::kind`).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use frances_models_ui::{
    ReasoningState, SectionApply, SectionId, SectionKind, ShellState, Source, WireSectionEvent,
};
pub use frances_workflow::SurfaceCmd;

use crate::llm::Usage;

pub use frances_workflow::permission::{
    PermissionRequest, PermissionResponse, PermissionResponseWire,
};

/// Persistence-side block identity. The on-disk `scrollback_blocks`
/// schema (today) is keyed by a per-row autoincrement; this type just
/// scopes the BlockId u64 within session APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlockId(pub u64);

impl std::fmt::Display for BlockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Persistence-side kind discriminator. Used by [`crate::scrollback`]
/// to encode rows; the live channel vocabulary uses [`SectionKind`]
/// directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockKind {
    Text {
        source: Source,
    },
    ToolUse {
        name: Arc<str>,
        detail: Option<Arc<str>>,
    },
    Tailed {
        header: TailedHeader,
    },
    Diff {
        lines: Vec<DiffLine>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TailedHeader {
    Shell { state: ShellState, cmd: Arc<str> },
    Reasoning { state: ReasoningState },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLine {
    Context { text: Arc<str>, line: u32 },
    Added(Arc<str>),
    Removed(Arc<str>),
}

#[derive(Debug)]
pub enum StreamFrame {
    /// Self-describing section content. The first append with a
    /// previously-unseen `id` implicitly opens the section (the UI's
    /// dispatcher constructs it via `make_section(&kind)`); subsequent
    /// appends either grow the text or carry an unchanged delta + new
    /// kind for metadata transitions (e.g. ShellState `Running` →
    /// `Success`).
    SectionAppend {
        id: SectionId,
        kind: SectionKind,
        delta: String,
    },
    /// Workflow sealed the section. The UI's dispatcher seals its
    /// trait object and routes a `Close` apply.
    SectionClose {
        id: SectionId,
    },
    /// Replay-only sibling of `SectionClose`: the section was in
    /// flight when its workflow was dehydrated, so it never received
    /// a clean close. The session runtime emits this in place of
    /// `SectionClose` from [`crate::scrollback::replay_to_channel`]
    /// for rows whose `truncated` column is set.
    SectionTruncated {
        id: SectionId,
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
    /// and carries its own section frames — a closed, bounded set
    /// distinct from the live variants above, so the UI's replay
    /// handler never has to reason about live-only frames.
    Scrollback(ScrollbackFrame),
}

/// The scrollback-replay sub-protocol: exactly the frames a replay
/// burst emits, in their own type so consumers match a small, real
/// set with no defensive arms. Produced by
/// [`crate::scrollback::replay_to_channel`].
#[derive(Debug, Clone)]
pub enum ScrollbackFrame {
    /// Burst opener: the UI clears its in-memory scrollback container
    /// and replays `instance_id`'s persisted history into the alt-screen
    /// inspector's committed deque.
    Reset { instance_id: Uuid },
    /// A persisted section's content (same shape as a live
    /// [`StreamFrame::SectionAppend`]). Today each persisted row
    /// replays as exactly one Append + one Close/Truncated.
    SectionAppend {
        id: SectionId,
        kind: SectionKind,
        delta: String,
    },
    /// Clean end of a persisted section.
    SectionClose { id: SectionId },
    /// Truncated end — the section was in flight when its workflow
    /// was dehydrated and never received a clean close.
    SectionTruncated { id: SectionId },
    /// A persisted error row.
    Error(String),
    /// Burst closer: the UI returns to live-mode handling.
    End,
}
