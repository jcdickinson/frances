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
//!   etc.); the frontend picks a rendering by kind on first appearance
//!   of a new section id.
//! - [`SectionId`] — per-invocation section identity.
//! - [`ReasoningState`] — completion-status enum carried inside the
//!   matching [`SectionKind`] variant.

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

/// Section identity, scoped to one workflow invocation. Monotonically
/// assigned by `transcript.push` on the workflow side. The frontend
/// uses it to route subsequent events to the right rendered section.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(transparent)]
pub struct SectionId(pub u64);

/// What kind of section, and any bounded metadata that rides with it.
/// One variant per section presentation in the UI. The frontend
/// matches on this to pick a rendering when a new section id is seen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SectionKind {
    /// `ErrorSection` — one-shot error message.
    Error,
    /// `ToolUseSection` — one-shot "→ tool_name" marker. `detail` is
    /// the optional human-readable suffix produced by the tool's
    /// `describe(call)` method.
    ToolUse {
        name: String,
        detail: Option<String>,
    },
    /// `JsonSection` — single tagged JSON value. Immutable after push.
    Json {
        tag: String,
        value: serde_json::Value,
    },
    /// `ReasoningSection` — streaming model reasoning. `state`
    /// transitions `Streaming → Done` on close.
    Reasoning { state: ReasoningState },
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

/// Terminal status for [`SectionKind::Reasoning`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub enum ReasoningState {
    Streaming,
    Done,
}
