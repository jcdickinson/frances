use frances_models_llm::ToolCall;
use tokio::sync::oneshot;

/// A permission request emitted on the agent's `permissions` output.
/// Carries its own reply slot — whoever answers resolves [`Self::reply`]
/// directly; there is no id-keyed correlation table.
///
/// `allow_auto` flags the gate as eligible for the host's auto-approver.
/// The UI ignores it (it only renders `prompt` / `tool_call`).
#[derive(Debug)]
pub struct PermissionRequest {
    /// Human-readable summary; the agent precomputes it so the UI
    /// doesn't need per-tool rendering logic.
    pub prompt: String,
    /// Structured subject — what tool invocation triggered the request,
    /// if any. Optional because a agent may gate something that
    /// isn't a tool call.
    pub tool_call: Option<ToolCall>,
    /// Whether the host's auto-approver may answer this gate.
    pub allow_auto: bool,
    /// Reply slot — the answerer sends the agent's `Yes`/`No` here.
    pub reply: oneshot::Sender<PermissionResponse>,
}

/// What the UI sends back over the wire. Three variants: yes / no /
/// user-redirected-to-chat. The runtime strips `RedirectToChat` before
/// resolving the agent's oneshot.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionResponseWire {
    Yes { details: Option<String> },
    No { details: Option<String> },
    RedirectToChat { content: String },
}

/// What the agent's oneshot resolves to. Just yes/no — redirect is
/// handled session-runtime-side.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionResponse {
    Yes { details: Option<String> },
    No { details: Option<String> },
}
