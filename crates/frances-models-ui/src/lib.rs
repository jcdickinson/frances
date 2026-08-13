//! Shared UI event vocabulary.
//!
//! Pure data crate. No logic, no deps traits — same role
//! `frances-models-llm` plays for the chat session surface. Both
//! `frances-workflow` (producer of sections via the JS API) and
//! the application frontend (consumer via the section dispatcher) depend on
//! this crate.
//!
//! What lives here:
//!
//! - [`SectionKind`] — the typed payload that identifies + describes a
//!   section. Workflows construct it via JS classes (`MarkdownSection`
//!   etc.); the UI dispatches by kind on first appearance
//!   of a new section id.
//! - [`Source`] — who produced a Markdown section (User / Assistant /
//!   Internal). Drives both the rendered sigil and the inline-markdown
//!   parser gate (`source != User`).
//! - [`SectionId`] — per-invocation section identity.
//! - [`ShellState`] / [`ReasoningState`] — completion-status enums carried
//!   inside the matching [`SectionKind`] variants.
//! - [`WireSectionEvent`] — what flows through the runtime → UI
//!   channel: self-describing `SectionAppend { id, kind, delta }` plus
//!   `SectionClose` / `SectionTruncated`.
//! - [`SectionApply`] — what the `Section` trait's `apply` method
//!   receives (no `Open` variant; first-Append-as-construct is the
//!   dispatcher's concern). The borrowed variant of [`WireSectionEvent`].

use serde::{Deserialize, Serialize};

/// Section identity, scoped to one workflow invocation. Monotonically
/// assigned by `transcript.push` on the workflow side. The UI uses it
/// to route subsequent events to the right `Box<dyn Section>`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SectionId(pub u64);

/// Who produced a [`SectionKind::Markdown`] section. Drives the host-
/// side sigil (`User` → `>`, `Assistant` → `◆`, `Internal` → none) and
/// gates inline markdown parsing (`source != User`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    User,
    Assistant,
    Internal,
}

/// What kind of section, and any bounded metadata that rides with it.
/// One variant per section presentation in the UI. The dispatcher
/// matches on this to pick the impl when a new section id is seen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SectionKind {
    /// `MarkdownSection` — streaming text. `source` names the speaker;
    /// the mdast parser runs for `source != User` with full inline
    /// styling; User source renders plain text. The container expands
    /// the section into `MarkdownBlock`s, one per top-level AST node.
    Markdown { source: Source },
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
    /// `ShellOutputSection` — streaming output from one shell command.
    /// `cmd` is pinned in the header. `state` transitions
    /// `Running → Success`/`Exit(N)` via a metadata-only Append (empty
    /// delta + new kind).
    ShellOutput { state: ShellState, cmd: String },
    /// `ReasoningSection` — streaming model reasoning. `state`
    /// transitions `Streaming → Done` on close.
    Reasoning { state: ReasoningState },
    /// `DiffSection` — one-shot structured diff produced by a file-
    /// edit tool.
    Diff { lines: Vec<frances_edit::DiffOp> },
}

/// Terminal status for [`SectionKind::ShellOutput`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellState {
    Running,
    Success,
    Exit(i32),
}

/// Terminal status for [`SectionKind::Reasoning`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningState {
    Streaming,
    Done,
}

/// What flows through the runtime → UI channel. Self-describing:
/// every [`WireSectionEvent::SectionAppend`] carries the section's
/// current `kind`, so any consumer can construct or update from a
/// single delta without having seen previous ones. The first Append
/// with an unseen id implicitly constructs the section (the UI's
/// dispatcher calls `make_section(&kind)`); subsequent Appends either
/// grow the text or carry an unchanged delta + new kind for metadata
/// transitions (e.g. shell `Running` → `Success`).
#[derive(Debug, Clone)]
pub enum WireSectionEvent {
    SectionAppend {
        id: SectionId,
        kind: SectionKind,
        delta: String,
    },
    SectionClose {
        id: SectionId,
    },
    /// Replay-only sibling of `SectionClose`: section was in flight
    /// when its workflow was dehydrated. The committed section view
    /// will be flagged truncated.
    SectionTruncated {
        id: SectionId,
    },
}

/// What the `Section` trait's `apply` method receives. Borrowed
/// variant of [`WireSectionEvent`] minus the `id` (which the
/// dispatcher uses to route, not the section impl to consume). No
/// `Open` variant: the first Append-with-an-unseen-id triggers
/// construction via the free `make_section` factory, then the same
/// Append is dispatched to `apply` so the section absorbs its initial
/// delta uniformly.
#[derive(Debug, Clone, Copy)]
pub enum SectionApply<'a> {
    Append {
        kind: &'a SectionKind,
        delta: &'a str,
    },
    Close,
    Truncate,
}
