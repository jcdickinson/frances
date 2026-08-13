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
//!   section. Workflows construct it via JS classes (`MarkdownSection`
//!   etc.); the frontend picks a rendering by kind on first appearance
//!   of a new section id.
//! - [`Source`] — who produced a Markdown section (User / Assistant /
//!   Internal). The frontend styles each speaker differently and only
//!   renders markdown for `source != User`.
//! - [`SectionId`] — per-invocation section identity.
//! - [`ShellState`] / [`ReasoningState`] — completion-status enums carried
//!   inside the matching [`SectionKind`] variants.

use serde::{Deserialize, Serialize};

/// Section identity, scoped to one workflow invocation. Monotonically
/// assigned by `transcript.push` on the workflow side. The frontend
/// uses it to route subsequent events to the right rendered section.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SectionId(pub u64);

/// Who produced a [`SectionKind::Markdown`] section. The frontend
/// styles each speaker differently and gates markdown rendering
/// (`source != User`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    User,
    Assistant,
    Internal,
}

/// What kind of section, and any bounded metadata that rides with it.
/// One variant per section presentation in the UI. The frontend
/// matches on this to pick a rendering when a new section id is seen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SectionKind {
    /// `MarkdownSection` — streaming text. `source` names the speaker;
    /// the frontend renders markdown for `source != User` and plain
    /// text for User source.
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
