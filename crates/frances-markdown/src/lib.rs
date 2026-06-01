//! Streaming markdown for the TUI's section dispatcher.
//!
//! The crate owns three things:
//!
//! - [`ParagraphBlock`] — a leaf [`frances_tui::Block`] holding styled
//!   spans for one paragraph. The TUI's container renders one of these
//!   per `\n\n`-separated paragraph in a `MarkdownSection`.
//! - [`MarkdownSection`] — a state machine that consumes
//!   [`SectionApply`] events for a section whose
//!   [`SectionKind::Markdown`] kind is its identity. Emits a fresh
//!   list of [`ParagraphBlock`]s per apply.
//! - [`parse_inline`] — single-pass scanner over a paragraph's chars
//!   recognising CommonMark `**bold**` / `__bold__` and `*italic*` /
//!   `_italic_`. No nesting, no escapes, no headings, no lists, no
//!   code fences. The scanner only runs when `source != Source::User`;
//!   user-echo sections render literal so the user's `*.rs files`
//!   stays visible.

mod inline;
mod paragraph_block;
mod section;

pub use inline::{StyledSpan, parse_inline};
pub use paragraph_block::ParagraphBlock;
pub use section::MarkdownSection;
