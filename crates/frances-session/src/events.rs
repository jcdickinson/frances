//! Stream-event surface shared by producers (workflows, scrollback
//! replay) and the UI consumer.
//!
//! These types travel through an in-process `tokio::sync::mpsc` from
//! the session runtime to the event bridge in the `frances` binary,
//! which re-shapes them into serialized `UiEvent`s for the webview.
//! The Tauri IPC edge is the bridge's concern; within this crate it's
//! a channel of Rust enums, not a wire format. The term "wire" is
//! reserved for the LLM provider's HTTP boundary (see
//! `frances-llm::Provider::kind`).

use uuid::Uuid;

pub use frances_models_ui::{
    EntityEnvelope, Lifecycle, ReasoningState, SectionId, SectionKind, Source,
};

pub use frances_workflow::permission::{
    PermissionRequest, PermissionResponse, PermissionResponseWire,
};

#[derive(Debug)]
pub enum StreamFrame {
    /// Self-describing section content. The first append with a
    /// previously-unseen `id` implicitly opens the section; subsequent
    /// appends either grow the text or carry an unchanged delta + new
    /// kind for metadata transitions (e.g. ReasoningState `Streaming`
    /// → `Done`).
    SectionAppend {
        id: SectionId,
        kind: SectionKind,
        delta: String,
    },
    /// Workflow sealed the section.
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
    /// Whole-entity upsert: latest-wins envelope + opaque snapshot
    /// published by the [`crate::entities::EntityHub`]. The hub queues
    /// one upsert per entity at runtime start (the attach snapshot)
    /// and re-emits the full pair on every change.
    EntityUpsert {
        envelope: EntityEnvelope,
        snapshot: serde_json::Value,
    },
    /// One item of an entity's append-only stream. Emitted only while
    /// the frontend is subscribed to that entity (live tail or the
    /// catch-up replay a subscription starts with); `seq` lets the
    /// consumer drop duplicates across the catch-up/live splice.
    EntityStream {
        entity_id: Uuid,
        seq: u64,
        payload: serde_json::Value,
    },
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
    /// Burst opener: the UI clears its rendered sections and replays
    /// `instance_id`'s persisted history from scratch.
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
