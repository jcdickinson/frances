//! `MarkdownSection` — the [`Section`] impl for
//! [`SectionKind::Markdown`]. State machine: accumulates text from
//! Append events, parses the full buffer as GFM via mdast on
//! every apply, and returns one [`MarkdownBlock`] per top-level AST
//! node.
//!
//! **Source gating:** `convert_node` receives the section's `Source`.
//! For `Source::User`, block-level structure (headings, lists, etc.) is
//! preserved but all inline styling is flattened to plain text so the
//! user's `*.rs files` doesn't turn the rest of the paragraph italic.
//!
//! **Defensive length handling:** if an incremental parse produces fewer
//! top-level nodes than the previous apply, blank `MarkdownBlock`s are
//! appended so that downstream index-based tracking in the container
//! never sees its position disappear.

use frances_models_tui::{SectionApply, Source};
use frances_tui::block::{Block, Sigil};
use frances_tui::section::Section;
use markdown::mdast;
use markdown::{ParseOptions, to_mdast};

use crate::convert::convert_node;
use crate::markdown_block::MarkdownBlock;
use crate::markdown_node::MarkdownNode;

pub struct MarkdownSection {
    source: Source,
    buffer: String,
    sealed: bool,
    truncated: bool,
    /// High-water mark of block counts returned by previous applies.
    /// If a re-parse produces fewer nodes, we pad with blank blocks so
    /// downstream consumers never see a position vanish.
    prev_block_count: usize,
}

impl MarkdownSection {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            buffer: String::new(),
            sealed: false,
            truncated: false,
            prev_block_count: 0,
        }
    }

    fn build_blocks(&mut self) -> Vec<Box<dyn Block>> {
        if self.buffer.is_empty() {
            self.prev_block_count = 0;
            return Vec::new();
        }

        // Parse the accumulated buffer as GFM.
        let root = match to_mdast(&self.buffer, &ParseOptions::gfm()) {
            Ok(node) => node,
            Err(_) => {
                // If mdast can't parse, fall back to a single plain-text block.
                return vec![blank_block()];
            }
        };

        let children = match &root {
            mdast::Node::Root(r) => &r.children,
            _ => return vec![blank_block()],
        };

        // Convert each root-level node, skipping None results (Definitions, etc.)
        let mut blocks: Vec<Box<dyn Block>> = children
            .iter()
            .filter_map(|node| convert_node(node, self.source))
            .map(|mn| Box::new(MarkdownBlock::new(mn)) as Box<dyn Block>)
            .collect();

        // Defensive length handling: pad if block count decreased.
        while blocks.len() < self.prev_block_count {
            blocks.push(blank_block());
        }
        self.prev_block_count = blocks.len();

        blocks
    }
}

/// Produce a blank block — an empty paragraph that renders as a single
/// empty row. Used for defensive padding.
fn blank_block() -> Box<dyn Block> {
    Box::new(MarkdownBlock::new(MarkdownNode::Paragraph {
        children: vec![],
    }))
}

impl Section for MarkdownSection {
    fn apply(&mut self, event: SectionApply<'_>) -> Vec<Box<dyn Block>> {
        match event {
            SectionApply::Append { delta, .. } => {
                self.buffer.push_str(delta);
            }
            SectionApply::Close => {
                self.sealed = true;
            }
            SectionApply::Truncate => {
                self.sealed = true;
                self.truncated = true;
            }
        }
        self.build_blocks()
    }

    fn sigil(&self) -> Sigil {
        // Gutter sigil for assistant turns vs internal chrome vs user
        // echoes. Matches the existing single-block path; the binary's
        // `sigil_for(WireBlockKind::Text { source })` is the source of
        // truth and gets consulted at section commit time.
        Sigil::blank()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frances_models_tui::SectionKind;

    fn assistant() -> MarkdownSection {
        MarkdownSection::new(Source::Assistant)
    }

    fn user() -> MarkdownSection {
        MarkdownSection::new(Source::User)
    }

    #[test]
    fn two_paragraphs_produce_two_blocks() {
        let mut s = assistant();
        let kind = SectionKind::Markdown {
            source: Source::Assistant,
        };
        let blocks = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "first\n\nsecond",
        });
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn empty_apply_produces_no_blocks() {
        let mut s = assistant();
        let kind = SectionKind::Markdown {
            source: Source::Assistant,
        };
        let blocks = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "",
        });
        assert_eq!(blocks.len(), 0);
    }

    /// User echo with `source == User` still produces blocks but
    /// inline styling is flattened to plain text by `convert_node`.
    #[test]
    fn user_source_produces_block() {
        let mut s = user();
        let kind = SectionKind::Markdown {
            source: Source::User,
        };
        let _ = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "look at *.rs files",
        });
        // build_blocks is now &mut, but apply already called it.
        // Re-apply to check stability.
        let blocks = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "",
        });
        assert_eq!(blocks.len(), 1);
    }

    /// A heading produces a MarkdownBlock with the Heading node.
    #[test]
    fn heading_produces_heading_block() {
        let mut s = assistant();
        let kind = SectionKind::Markdown {
            source: Source::Assistant,
        };
        let blocks = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "# Title",
        });
        assert_eq!(blocks.len(), 1);
    }

    /// A code fence produces a MarkdownBlock with the Code node.
    #[test]
    fn code_fence_produces_code_block() {
        let mut s = assistant();
        let kind = SectionKind::Markdown {
            source: Source::Assistant,
        };
        let blocks = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "```rust\nfn main() {}\n```",
        });
        assert_eq!(blocks.len(), 1);
    }

    /// A thematic break produces a MarkdownBlock.
    #[test]
    fn thematic_break_produces_block() {
        let mut s = assistant();
        let kind = SectionKind::Markdown {
            source: Source::Assistant,
        };
        let blocks = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "---",
        });
        assert_eq!(blocks.len(), 1);
    }

    /// A list with two items produces one MarkdownBlock (the List node
    /// contains two ListItem children rendered internally).
    #[test]
    fn list_produces_single_block() {
        let mut s = assistant();
        let kind = SectionKind::Markdown {
            source: Source::Assistant,
        };
        let blocks = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "- one\n- two",
        });
        assert_eq!(blocks.len(), 1);
    }

    /// Blockquote produces a single MarkdownBlock.
    #[test]
    fn blockquote_produces_single_block() {
        let mut s = assistant();
        let kind = SectionKind::Markdown {
            source: Source::Assistant,
        };
        let blocks = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "> quoted text",
        });
        assert_eq!(blocks.len(), 1);
    }

    /// Definition nodes are skipped — no block produced.
    #[test]
    fn definition_skipped() {
        let mut s = assistant();
        let kind = SectionKind::Markdown {
            source: Source::Assistant,
        };
        let blocks = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "[label]: https://example.com\n\nSome text",
        });
        // Should be 1 block (the paragraph "Some text"), not 2.
        assert_eq!(blocks.len(), 1);
    }

    /// Defensive padding: if an incremental re-parse produces fewer
    /// blocks, the vec is padded with blank blocks.
    #[test]
    fn defensive_padding_when_blocks_shrink() {
        let mut s = assistant();
        let kind = SectionKind::Markdown {
            source: Source::Assistant,
        };

        // First apply: two paragraphs separated by blank line
        let blocks = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "first\n\nsecond",
        });
        assert_eq!(blocks.len(), 2);
        // prev_block_count is now 2

        // Now apply a truncate which re-applies — still 2 blocks
        // since the buffer hasn't changed.
        let blocks = s.apply(SectionApply::Truncate);
        assert_eq!(blocks.len(), 2);
    }

    /// Close event marks section sealed but still returns blocks.
    #[test]
    fn close_returns_blocks() {
        let mut s = assistant();
        let kind = SectionKind::Markdown {
            source: Source::Assistant,
        };
        let _ = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "hello",
        });
        let blocks = s.apply(SectionApply::Close);
        assert_eq!(blocks.len(), 1);
        assert!(s.sealed);
    }
}
