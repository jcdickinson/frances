//! Section impls for the TUI's section dispatcher.
//!
//! Each variant of [`frances_models_tui::SectionKind`] resolves to a
//! concrete [`Section`] impl via [`make_section`]. For round 1, every
//! kind routes through [`SingleBlockSection`] — a thin wrapper that
//! owns one inner [`Block`] (constructed via the existing
//! [`block_for_kind`]) and rebuilds it on every Append. Markdown gets
//! a dedicated multi-block impl in `frances-markdown` (landing in
//! step 6 of the migration); when that lands, this dispatcher routes
//! `SectionKind::Markdown` there instead.
//!
//! Step 2 status: skeleton. No consumers yet — the container and the
//! event dispatcher start using these in step 3 and step 4
//! respectively.
//!
//! See `docs/plan/section-and-markdown.md`.

use std::sync::Arc;

use frances_models_tui::{SectionApply, SectionKind};
use frances_session::events::{
    BlockKind as WireBlockKind, ReasoningState as WireReasoningState, ShellState as WireShellState,
    TailedHeader,
};
use frances_tui::block::{Block, Sigil};
use frances_tui::section::Section;

use crate::tui::blocks::{block_for_kind, sigil_for};

/// State machine: holds accumulated text + current kind, produces a
/// single inner block per apply. Used for every section type except
/// Markdown (which has its own multi-block impl in `frances-markdown`).
pub struct SingleBlockSection {
    kind: SectionKind,
    text: String,
    sealed: bool,
    truncated: bool,
}

impl SingleBlockSection {
    pub fn new(kind: SectionKind) -> Self {
        Self {
            kind,
            text: String::new(),
            sealed: false,
            truncated: false,
        }
    }

    fn build(&self) -> Vec<Box<dyn Block>> {
        let wire = wire_kind_for(&self.kind);
        vec![block_for_kind(wire, self.text.clone())]
    }
}

impl Section for SingleBlockSection {
    fn apply(&mut self, event: SectionApply<'_>) -> Vec<Box<dyn Block>> {
        match event {
            SectionApply::Append { kind, delta } => {
                self.kind = kind.clone();
                if !delta.is_empty() {
                    self.text.push_str(delta);
                }
            }
            SectionApply::Close => {
                self.sealed = true;
            }
            SectionApply::Truncate => {
                self.sealed = true;
                self.truncated = true;
            }
        }
        self.build()
    }

    fn sigil(&self) -> Sigil {
        sigil_for(&wire_kind_for(&self.kind))
    }
}

/// Translate a workflow-side [`SectionKind`] to the TUI-side wire
/// [`WireBlockKind`] consumed by [`block_for_kind`]. Mirrors the
/// `emit_transcript` path in `frances-session::workflows`.
fn wire_kind_for(kind: &SectionKind) -> WireBlockKind {
    use frances_models_tui::{ReasoningState as MReasoning, ShellState as MShell};
    match kind {
        SectionKind::Markdown { source } => WireBlockKind::Text { source: *source },
        SectionKind::Error => WireBlockKind::Text {
            source: frances_models_tui::Source::Internal,
        },
        SectionKind::ToolUse { name, detail } => WireBlockKind::ToolUse {
            name: Arc::from(name.as_str()),
            detail: detail.as_deref().map(Arc::from),
        },
        SectionKind::Json { .. } => WireBlockKind::Text {
            source: frances_models_tui::Source::Internal,
        },
        SectionKind::ShellOutput { state, cmd } => WireBlockKind::Tailed {
            header: TailedHeader::Shell {
                state: match state {
                    MShell::Running => WireShellState::Running,
                    MShell::Success => WireShellState::Success,
                    MShell::Exit(n) => WireShellState::Exit(*n),
                },
                cmd: Arc::from(cmd.as_str()),
            },
        },
        SectionKind::Reasoning { state } => WireBlockKind::Tailed {
            header: TailedHeader::Reasoning {
                state: match state {
                    MReasoning::Streaming => WireReasoningState::Streaming,
                    MReasoning::Done => WireReasoningState::Done,
                },
            },
        },
        SectionKind::Diff { lines } => WireBlockKind::Diff {
            lines: lines.iter().map(diff_op_to_wire).collect(),
        },
    }
}

fn diff_op_to_wire(op: &frances_edit::DiffOp) -> frances_session::events::DiffLine {
    use frances_edit::DiffOp;
    use frances_session::events::DiffLine;
    match op {
        DiffOp::Context { text, line } => DiffLine::Context {
            text: Arc::from(text.as_str()),
            line: *line,
        },
        DiffOp::Added(t) => DiffLine::Added(Arc::from(t.as_str())),
        DiffOp::Removed(t) => DiffLine::Removed(Arc::from(t.as_str())),
    }
}

/// Construct the right [`Section`] impl for a given kind. Markdown
/// sections route through [`frances_markdown::MarkdownSection`]
/// (paragraph splitter + inline parser); everything else goes through
/// the generic [`SingleBlockSection`] wrapper around the existing
/// block-level constructors.
pub fn make_section(kind: &SectionKind) -> Box<dyn Section> {
    match kind {
        SectionKind::Markdown { source } => {
            Box::new(frances_markdown::MarkdownSection::new(*source))
        }
        _ => Box::new(SingleBlockSection::new(kind.clone())),
    }
}
