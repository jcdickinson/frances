//! Permission gate primitives.
//!
//! A workflow asks the user "may I do this?" and waits for an answer.
//! The shapes live in this crate because both the JS module
//! (`frances:v1/approval`) and the host emit/consume them; the runtime
//! re-exports `PermissionRequest` / `PermissionResponseWire` over its
//! wire protocol verbatim.
//!
//! The wire response (`PermissionResponseWire`) and the workflow-facing
//! response (`PermissionResponse`) deliberately differ. The wire form
//! includes `RedirectToChat { content }` for the case where the user
//! types text instead of yes/no; the runtime strips that variant — the
//! workflow only ever sees `Yes` / `No`. Keeping the two types separate
//! means a script can never be handed a "response" it can't sanely
//! consume.
//!
//! Serialization note: the runtime ↔ TUI wire is bincode, whose serde
//! adapter is not self-describing and rejects internally-tagged enums
//! (`#[serde(tag = "...")]`) with `Serde(AnyNotSupported)`. So the
//! enums here use externally-tagged form (serde's default). The JS
//! bridge does not go through serde — it has its own `IntoJs` impls
//! that emit the `{ type, ... }` shape the JS module expects.

use frances_models_llm::wire::ToolCall;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// Identifies a single permission round-trip. Assigned by the gateway
/// when the workflow allocates a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermissionId(pub u64);

impl std::fmt::Display for PermissionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The wire-bound permission request. The TUI sees exactly this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: PermissionId,
    /// Human-readable summary; the workflow precomputes it so the TUI
    /// doesn't need per-tool rendering logic.
    pub prompt: String,
    /// Structured subject — what tool invocation triggered the request,
    /// if any. Optional because a workflow may gate something that
    /// isn't a tool call.
    pub tool_call: Option<ToolCall>,
}

/// What the TUI sends back over the wire. Three variants: yes / no /
/// user-redirected-to-chat. The runtime strips `RedirectToChat` before
/// resolving the workflow's oneshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PermissionResponseWire {
    Yes { details: Option<String> },
    No { details: Option<String> },
    RedirectToChat { content: String },
}

/// What the workflow's oneshot resolves to. Just yes/no — redirect is
/// handled session-runtime-side.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionResponse {
    Yes { details: Option<String> },
    No { details: Option<String> },
}

/// Host-side handle the workflow uses to ask the user a question and
/// wait for their answer.
///
/// Daemon impl: shared `Arc<DashMap<PermissionId, oneshot::Sender>>`
/// also held by the RPC handler that the TUI calls into. Tests can
/// stub it however they want.
pub trait Permissions: Clone + Send + Sync + 'static {
    /// Reserve a fresh id + register a pending response slot. The
    /// caller emits the returned `PermissionRequest` to the host and
    /// awaits the receiver.
    ///
    /// `allow_auto` is not handled here — it rides on the
    /// `HostFrame::Permission` variant the caller emits, since only
    /// the host frame's consumer (the runtime's emit loop) reads it.
    fn allocate(
        &self,
        prompt: String,
        tool_call: Option<ToolCall>,
    ) -> (PermissionRequest, oneshot::Receiver<PermissionResponse>);
}
