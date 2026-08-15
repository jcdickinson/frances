//! Shared UI event vocabulary.
//!
//! Pure data crate. No logic, no deps traits — same role
//! `frances-models-llm` plays for the chat session surface. Both
//! `frances-workflow` (producer of sections via the JS API) and the
//! `frances` binary's event bridge (which serializes sections to the
//! frontend) depend on this crate.
//!
//! What lives here:
//!
//! - [`SectionKind`] — the typed payload that identifies + describes a
//!   section. Workflows construct it via JS classes (`ErrorSection`
//!   etc.); the frontend picks a rendering by kind.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Whether an entity's producer is still running. The only envelope
/// field besides identity: core machinery (forced settle, subscription
/// gating) reads it, so it lives outside the opaque snapshot payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Live,
    Settled,
}

/// The typed half of an entity. Everything core Rust reads lives here;
/// the snapshot payload riding next to it is opaque JSON interpreted
/// only by the producer and the frontend's per-kind components. If core
/// logic ever needs a field from the payload, that field moves into the
/// envelope instead of core parsing the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct EntityEnvelope {
    pub entity_id: Uuid,
    /// Open set — producers (including JS workflows) mint kinds freely;
    /// core never matches on it, the frontend dispatches renderers by it.
    pub kind: String,
    pub lifecycle: Lifecycle,
}

/// What kind of section, and the data that rides with it. Every
/// section is one-shot: the workflow pushes it fully formed and the
/// frontend matches on this to pick a rendering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SectionKind {
    /// `ErrorSection` — one-shot error message. Side-channel: the
    /// session driver turns it into an error frame rather than a
    /// rendered section.
    Error { text: String },
    /// `JsonSection` — single tagged JSON value.
    Json {
        tag: String,
        value: serde_json::Value,
    },
    /// `DiffSection` — one-shot structured diff produced by a file-
    /// edit tool.
    Diff { lines: Vec<frances_edit::DiffOp> },
    /// `EntityRefSection` — one-shot pointer at an entity. The
    /// transcript carries only the reference; the entity's snapshot
    /// (and, on demand, its stream) render it. Dumb by design: the
    /// entity exists independently via the registry/hub, refs are
    /// optional decoration.
    EntityRef { entity_id: Uuid },
}
