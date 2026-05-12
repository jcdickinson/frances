//! Approval gate primitives.
//!
//! A workflow asks the user a yes/no/chat question, and waits for an
//! answer. The shapes live in this crate because both the JS module
//! (`frances:v1/approval`) and the host emit/consume them; the daemon
//! re-exports `ApprovalRequest` / `ApprovalChoice` over its wire
//! protocol verbatim.
//!
//! Forward-looking: `ApprovalKind` is an enum so multi-choice and
//! richer prompt shapes can land without breaking callers. `v1` only
//! emits `YesNo`.
//!
//! Serialization note: the daemon ↔ TUI wire is bincode, whose serde
//! adapter is not self-describing and rejects internally-tagged enums
//! (`#[serde(tag = "...")]`) with `Serde(AnyNotSupported)`. So the
//! enums here use externally-tagged form (serde's default). The JS
//! bridge does not go through serde — it has its own `IntoJs` impls
//! that emit the `{ type, ... }` shape the JS module expects.

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// Identifies a single approval round-trip. Assigned by the gateway
/// when the workflow allocates a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApprovalId(pub u64);

impl std::fmt::Display for ApprovalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The full question we send to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: ApprovalId,
    /// Plain text shown to the user.
    pub prompt: String,
    /// Shape of the response set. v1 only emits `YesNo`; richer
    /// variants land here without breaking existing call sites.
    pub kind: ApprovalKind,
}

/// Shape of the choices on offer. v1 has one variant; future variants
/// (`Choice { options: Vec<String> }`, etc.) get added here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalKind {
    /// Plain yes/no, with a "chat" escape hatch for free-form text.
    YesNo,
}

/// The user's answer. `Yes`/`No` carry optional free-form details so
/// callers can capture reasoning ("yes, but only for this file");
/// `Chat` collapses approval into a normal chat message instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalChoice {
    Yes { details: Option<String> },
    No { details: Option<String> },
    Chat { content: String },
}

/// Host-side handle the workflow uses to ask the user a question and
/// wait for their answer.
///
/// Daemon impl: shared `Arc<DashMap<ApprovalId, oneshot::Sender<…>>>`
/// also held by the RPC handler that the TUI calls into. Tests can
/// stub it however they want.
pub trait ApprovalGateway: Clone + Send + Sync + 'static {
    /// Reserve a fresh id + register a pending response slot. The
    /// caller emits the returned `ApprovalRequest` to the host and
    /// awaits the receiver.
    fn allocate(
        &self,
        prompt: String,
        kind: ApprovalKind,
    ) -> (ApprovalRequest, oneshot::Receiver<ApprovalChoice>);
}
