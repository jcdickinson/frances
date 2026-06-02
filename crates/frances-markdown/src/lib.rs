//! Streaming markdown for the TUI's section dispatcher.
//!
//! The crate owns:
//!
//! - [`MarkdownBlock`] — a leaf [`frances_tui::Block`] that renders a single
//!   [`MarkdownNode`] (paragraph, heading, code, blockquote, list, etc.).
//! - [`MarkdownSection`] — a state machine that consumes [`SectionApply`]
//!   events, parses the accumulated buffer via mdast on every apply, and
//!   emits a fresh list of [`MarkdownBlock`]s.
//! - [`MarkdownNode`] — our own markdown AST mirroring the mdast structure.

mod convert;
mod markdown_block;
mod section;

pub mod markdown_node;

pub use markdown_block::MarkdownBlock;
pub use markdown_node::MarkdownNode;
pub use section::MarkdownSection;

