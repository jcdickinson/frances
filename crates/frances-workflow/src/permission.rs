//! Permission gate primitives.
//!
//! A workflow asks the user "may I do this?" and waits for an answer.
//! The shapes live in this crate because both the JS module
//! (`frances:v1/approval`) and the host emit/consume them.
//!
//! The request carries its own reply slot: the `frances:v1/approval`
//! primitive makes a `oneshot`, embeds the sender in the
//! [`PermissionRequest`], emits it on the workflow's `permissions`
//! output, and awaits the receiver. Whoever answers — the host's
//! auto-judge or the UI — resolves the embedded `reply`. There is no
//! id-keyed correlation table.
//!
//! The wire response (`PermissionResponseWire`) and the workflow-facing
//! response (`PermissionResponse`) deliberately differ. The wire form
//! includes `RedirectToChat { content }` for the case where the user
//! types text instead of yes/no; the host strips that variant — the
//! workflow only ever sees `Yes` / `No`. Keeping the two types separate
//! means a script can never be handed a "response" it can't sanely
//! consume. The JS bridge has its own `IntoJs` impls emitting the
//! `{ type, ... }` shape the JS module expects.

use frances_models_llm::ToolCall;
use tokio::sync::oneshot;

/// A permission request emitted on the workflow's `permissions` output.
/// Carries its own reply slot — whoever answers resolves [`Self::reply`]
/// directly; there is no id-keyed correlation table.
///
/// `allow_auto` flags the gate as eligible for the host's auto-approver.
/// The UI ignores it (it only renders `prompt` / `tool_call`).
#[derive(Debug)]
pub struct PermissionRequest {
    /// Human-readable summary; the workflow precomputes it so the UI
    /// doesn't need per-tool rendering logic.
    pub prompt: String,
    /// Structured subject — what tool invocation triggered the request,
    /// if any. Optional because a workflow may gate something that
    /// isn't a tool call.
    pub tool_call: Option<ToolCall>,
    /// Whether the host's auto-approver may answer this gate.
    pub allow_auto: bool,
    /// Reply slot — the answerer sends the workflow's `Yes`/`No` here.
    pub reply: oneshot::Sender<PermissionResponse>,
}

/// What the UI sends back over the wire. Three variants: yes / no /
/// user-redirected-to-chat. The runtime strips `RedirectToChat` before
/// resolving the workflow's oneshot.
#[derive(Debug, Clone, PartialEq)]
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
