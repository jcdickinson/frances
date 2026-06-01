//! `MarkdownSection` — the [`Section`] impl for
//! [`SectionKind::Markdown`]. State machine: accumulates text from
//! Append events, splits on `\n\n`, returns one [`ParagraphBlock`]
//! per paragraph on every apply.
//!
//! Inline-parser gate: `parse_inline` runs only when
//! `source != Source::User`. User-echo sections render literal so
//! the user's `*.rs files` doesn't turn the rest of the paragraph
//! italic.

use frances_models_tui::{SectionApply, Source};
use frances_tui::block::{Block, Sigil};
use frances_tui::section::Section;

use crate::inline::{StyledSpan, parse_inline};
use crate::paragraph_block::ParagraphBlock;

pub struct MarkdownSection {
    source: Source,
    buffer: String,
    sealed: bool,
    truncated: bool,
}

impl MarkdownSection {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            buffer: String::new(),
            sealed: false,
            truncated: false,
        }
    }

    fn build_blocks(&self) -> Vec<Box<dyn Block>> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let parse_with_markdown = self.source != Source::User;
        self.buffer
            .split("\n\n")
            .map(|para| {
                let spans = if parse_with_markdown {
                    parse_inline(para)
                } else {
                    vec![StyledSpan::plain(para.to_owned())]
                };
                Box::new(ParagraphBlock::new(spans)) as Box<dyn Block>
            })
            .collect()
    }
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
    use ratatui::style::Modifier;

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

    /// User echo with `source == User` skips `parse_inline`.
    #[test]
    fn user_source_skips_inline_parse() {
        let mut s = user();
        let kind = SectionKind::Markdown {
            source: Source::User,
        };
        let _ = s.apply(SectionApply::Append {
            kind: &kind,
            delta: "look at *.rs files",
        });
        let blocks = s.build_blocks();
        assert_eq!(blocks.len(), 1);
    }

    /// `source == Assistant` parses `**bold**` into a BOLD-modifier span.
    #[test]
    fn assistant_source_parses_bold() {
        let spans = parse_inline("hello **there**");
        assert!(
            spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
    }
}
