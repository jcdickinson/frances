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

pub use frances_models_ui::{EntityEnvelope, Lifecycle, SectionKind};

pub use frances_workflow::permission::{
    PermissionRequest, PermissionResponse, PermissionResponseWire,
};

#[derive(Debug)]
pub enum StreamFrame {
    /// A finished section. Sections are one-shot — everything the UI
    /// renders rides in `kind` — so there's nothing to open or seal.
    Section(SectionKind),
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
    /// A persisted section (same shape as a live
    /// [`StreamFrame::Section`]) — one frame per stored row.
    Section(SectionKind),
    /// A persisted error row.
    Error(String),
    /// Burst closer: the UI returns to live-mode handling.
    End,
}
