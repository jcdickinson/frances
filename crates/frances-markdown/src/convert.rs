//! Conversion from `mdast::Node` to `MarkdownNode`.
//!
//! The main entry point is [`convert_node`], which recursively maps a
//! mdast node tree into our own `MarkdownNode` enum. No third-party types
//! leak into the public API.
//!
//! **Source gating:** when `source == Source::User`, block-level structure
//! (headings, lists, blockquotes, code blocks) is preserved, but inline
//! styling (bold, italic, links, images, inline code) is flattened to
//! plain `Text` nodes so the rendering layer sees nothing to style.

use frances_models_tui::Source;
use markdown::mdast;

use crate::markdown_node::{MarkdownNode, MarkdownTable, TableAlignment, TableCell, TableRow};

// ── Public entry point ─────────────────────────────────────────────

/// Convert a top-level mdast node into a `MarkdownNode`.
///
/// Returns `None` for nodes that should be skipped entirely
/// (e.g. `Definition`) or for unrecognised node kinds.
pub fn convert_node(node: &mdast::Node, source: Source) -> Option<MarkdownNode> {
    match node {
        // ── Block-level ────────────────────────────────────────────
        mdast::Node::Paragraph(p) => Some(MarkdownNode::Paragraph {
            children: convert_inline_children(&p.children, source),
        }),

        mdast::Node::Heading(h) => Some(MarkdownNode::Heading {
            depth: h.depth,
            children: convert_inline_children(&h.children, source),
        }),

        mdast::Node::Code(c) => Some(MarkdownNode::Code {
            lang: c.lang.clone(),
            value: c.value.clone(),
        }),

        mdast::Node::Html(h) => Some(MarkdownNode::Html {
            value: h.value.clone(),
        }),

        mdast::Node::Blockquote(bq) => Some(MarkdownNode::Blockquote {
            children: convert_block_children(&bq.children, source),
        }),

        mdast::Node::List(l) => Some(MarkdownNode::List {
            ordered: l.ordered,
            start: l.start,
            children: convert_block_children(&l.children, source),
        }),

        mdast::Node::ListItem(li) => Some(MarkdownNode::ListItem {
            children: convert_block_children(&li.children, source),
        }),

        mdast::Node::ThematicBreak(_) => Some(MarkdownNode::ThematicBreak),

        mdast::Node::Table(table) => Some(MarkdownNode::Table(MarkdownTable {
            alignments: table.align.iter().map(convert_table_alignment).collect(),
            rows: convert_table_rows(&table.children, source),
        })),

        // ── Explicitly skipped ─────────────────────────────────────
        mdast::Node::Definition(_) => None,

        // ── Unrecognised / non-CommonMark ──────────────────────────
        _ => None,
    }
}

// ── Block children (Root, Blockquote, ListItem, List) ─────────────

fn convert_block_children(nodes: &[mdast::Node], source: Source) -> Vec<MarkdownNode> {
    nodes
        .iter()
        .filter_map(|n| convert_node(n, source))
        .collect()
}

fn convert_table_alignment(align: &mdast::AlignKind) -> TableAlignment {
    match align {
        mdast::AlignKind::None => TableAlignment::None,
        mdast::AlignKind::Left => TableAlignment::Left,
        mdast::AlignKind::Right => TableAlignment::Right,
        mdast::AlignKind::Center => TableAlignment::Center,
    }
}

fn convert_table_rows(nodes: &[mdast::Node], source: Source) -> Vec<TableRow> {
    nodes
        .iter()
        .filter_map(|node| match node {
            mdast::Node::TableRow(row) => Some(TableRow {
                cells: convert_table_cells(&row.children, source),
            }),
            _ => None,
        })
        .collect()
}

fn convert_table_cells(nodes: &[mdast::Node], source: Source) -> Vec<TableCell> {
    nodes
        .iter()
        .filter_map(|node| match node {
            mdast::Node::TableCell(cell) => Some(TableCell {
                children: convert_inline_children(&cell.children, source),
            }),
            _ => None,
        })
        .collect()
}

// ── Inline children (Paragraph, Heading) ──────────────────────────

/// Convert a slice of inline mdast nodes into `MarkdownNode`s.
///
/// For `Source::User`, all inline styling is flattened into plain
/// `Text` nodes. For other sources, each inline variant is converted
/// faithfully (Strong → Strong, Emphasis → Emphasis, etc.).
fn convert_inline_children(nodes: &[mdast::Node], source: Source) -> Vec<MarkdownNode> {
    if source == Source::User {
        let text = collect_plain_text(nodes);
        if text.is_empty() {
            vec![]
        } else {
            vec![MarkdownNode::Text { value: text }]
        }
    } else {
        nodes
            .iter()
            .filter_map(|n| convert_inline(n, source))
            .collect()
    }
}

/// Convert a single inline mdast node. Only called for non-User sources.
fn convert_inline(node: &mdast::Node, source: Source) -> Option<MarkdownNode> {
    match node {
        mdast::Node::Text(t) => Some(MarkdownNode::Text {
            value: t.value.clone(),
        }),

        mdast::Node::Strong(s) => Some(MarkdownNode::Strong {
            children: convert_inline_children(&s.children, source),
        }),

        mdast::Node::Emphasis(e) => Some(MarkdownNode::Emphasis {
            children: convert_inline_children(&e.children, source),
        }),

        mdast::Node::InlineCode(ic) => Some(MarkdownNode::InlineCode {
            value: ic.value.clone(),
        }),

        mdast::Node::Link(l) => Some(MarkdownNode::Link {
            url: l.url.clone(),
            title: l.title.clone(),
            children: convert_inline_children(&l.children, source),
        }),

        mdast::Node::Image(img) => Some(MarkdownNode::Image {
            url: img.url.clone(),
            alt: img.alt.clone(),
            title: img.title.clone(),
        }),

        mdast::Node::Break(_) => Some(MarkdownNode::Break),

        // Unrecognised inline — skip
        _ => None,
    }
}

// ── Plain-text collection (Source::User path) ─────────────────────

/// Recursively collect plain text from inline mdast nodes.
/// Used when `Source::User` to flatten all styling into literal text.
fn collect_plain_text(nodes: &[mdast::Node]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            mdast::Node::Text(t) => out.push_str(&t.value),
            mdast::Node::Strong(s) => out.push_str(&collect_plain_text(&s.children)),
            mdast::Node::Emphasis(e) => out.push_str(&collect_plain_text(&e.children)),
            mdast::Node::InlineCode(ic) => out.push_str(&ic.value),
            mdast::Node::Link(l) => {
                out.push_str(&collect_plain_text(&l.children));
            }
            mdast::Node::Image(img) => out.push_str(&img.alt),
            mdast::Node::Break(_) => out.push('\n'),
            // Skip unknown inline types
            _ => {}
        }
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use markdown::{ParseOptions, to_mdast};

    use crate::markdown_block::MarkdownBlock;
    use frances_tui::block::{Block, BlockRenderContext};
    use frances_tui::widget::{FrameTime, Theme};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Style};
    fn parse(text: &str) -> mdast::Node {
        to_mdast(text, &ParseOptions::gfm()).unwrap()
    }

    fn root_children(text: &str) -> Vec<mdast::Node> {
        match parse(text) {
            mdast::Node::Root(r) => r.children,
            _ => panic!("expected Root node"),
        }
    }

    fn convert_first(text: &str, source: Source) -> Option<MarkdownNode> {
        let children = root_children(text);
        children.first().and_then(|n| convert_node(n, source))
    }

    struct StubFrameTime;

    impl FrameTime for StubFrameTime {
        fn get_frame(&self) -> f64 {
            0.0
        }
    }

    fn render_node(node: MarkdownNode, width: u16, height: u16) -> Buffer {
        let block = MarkdownBlock::new(node);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        let theme = Theme::default();
        let ft = StubFrameTime;
        let mut render_ctx = BlockRenderContext {
            area,
            buf: &mut buf,
            src_y: 0,
            truncated: false,
            alt_view: false,
            selected: false,
            selected_part: None,
            theme: &theme,
            frame_time: &ft,
        };
        block.render(&mut render_ctx);
        buf
    }

    fn rendered_style_for_text(buf: &Buffer, y: u16, text: &str) -> Style {
        let row = (0..buf.area.width)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect::<Vec<_>>()
            .join("");
        let x = row
            .find(text)
            .unwrap_or_else(|| panic!("{text:?} not found in {row:?}"));
        buf.cell((x as u16, y)).unwrap().style()
    }

    fn assert_plain_style(style: Style) {
        assert_eq!(style.fg, Some(Color::Reset));
        assert!(style.add_modifier.is_empty());
    }

    // ── Block-level conversions ────────────────────────────────────

    #[test]
    fn paragraph() {
        let node = convert_first("hello world", Source::Assistant).unwrap();
        assert!(matches!(node, MarkdownNode::Paragraph { .. }));
        let MarkdownNode::Paragraph { children } = node else {
            unreachable!()
        };
        assert_eq!(children.len(), 1);
        assert_eq!(
            children[0],
            MarkdownNode::Text {
                value: "hello world".to_owned()
            }
        );
    }

    #[test]
    fn heading_depth_preserved() {
        let node = convert_first("## Title", Source::Assistant).unwrap();
        assert!(matches!(node, MarkdownNode::Heading { depth: 2, .. }));
    }

    #[test]
    fn heading_inline_children() {
        let node = convert_first("# Hello *world*", Source::Assistant).unwrap();
        let MarkdownNode::Heading { children, depth: _ } = node else {
            unreachable!()
        };
        // Should have Text("Hello ") and Emphasis
        assert!(
            children
                .iter()
                .any(|c| matches!(c, MarkdownNode::Emphasis { .. }))
        );
    }

    #[test]
    fn code_block_with_lang() {
        let node = convert_first("```rust\nfn main() {}\n```", Source::Assistant).unwrap();
        let MarkdownNode::Code { lang, value } = node else {
            unreachable!()
        };
        assert_eq!(lang.as_deref(), Some("rust"));
        assert_eq!(value, "fn main() {}");
    }

    #[test]
    fn code_block_without_lang() {
        let node = convert_first("```\nsome code\n```", Source::Assistant).unwrap();
        let MarkdownNode::Code { lang, value } = node else {
            unreachable!()
        };
        assert!(lang.is_none());
        assert_eq!(value, "some code");
    }

    #[test]
    fn html_node() {
        let node = convert_first("<div>\nhello\n</div>", Source::Assistant).unwrap();
        let MarkdownNode::Html { value } = node else {
            unreachable!()
        };
        assert!(value.contains("<div>"));
    }

    #[test]
    fn blockquote_with_paragraph() {
        let node = convert_first("> hello", Source::Assistant).unwrap();
        let MarkdownNode::Blockquote { children } = node else {
            unreachable!()
        };
        assert_eq!(children.len(), 1);
        assert!(matches!(children[0], MarkdownNode::Paragraph { .. }));
    }

    #[test]
    fn unordered_list() {
        let node = convert_first("- one\n- two", Source::Assistant).unwrap();
        let MarkdownNode::List {
            ordered,
            start,
            children,
        } = node
        else {
            unreachable!()
        };
        assert!(!ordered);
        assert!(start.is_none());
        assert_eq!(children.len(), 2);
        assert!(
            children
                .iter()
                .all(|c| matches!(c, MarkdownNode::ListItem { .. }))
        );
    }

    #[test]
    fn ordered_list_with_start() {
        let node = convert_first("3. gamma\n4. delta", Source::Assistant).unwrap();
        let MarkdownNode::List {
            ordered,
            start,
            children,
        } = node
        else {
            unreachable!()
        };
        assert!(ordered);
        assert_eq!(start, Some(3));
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn thematic_break() {
        let node = convert_first("---", Source::Assistant).unwrap();
        assert_eq!(node, MarkdownNode::ThematicBreak);
    }

    #[test]
    fn table_node() {
        let node = convert_first(
            "| Name | Value |\n| --- | --- |\n| one | two |",
            Source::Assistant,
        )
        .unwrap();
        let MarkdownNode::Table(table) = node else {
            unreachable!()
        };
        assert_eq!(
            table.alignments,
            vec![TableAlignment::None, TableAlignment::None]
        );
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0].cells.len(), 2);
        assert_eq!(
            table.rows[0].cells[0].children,
            vec![MarkdownNode::Text {
                value: "Name".to_owned()
            }]
        );
        assert_eq!(
            table.rows[1].cells[1].children,
            vec![MarkdownNode::Text {
                value: "two".to_owned()
            }]
        );
    }

    #[test]
    fn table_alignments_preserved() {
        let node = convert_first(
            "| A | B | C | D |\n| --- | :--- | ---: | :---: |\n| a | b | c | d |",
            Source::Assistant,
        )
        .unwrap();
        let MarkdownNode::Table(table) = node else {
            unreachable!()
        };
        assert_eq!(
            table.alignments,
            vec![
                TableAlignment::None,
                TableAlignment::Left,
                TableAlignment::Right,
                TableAlignment::Center,
            ]
        );
    }

    #[test]
    fn user_source_table_cells_flatten_inline_styling() {
        let node = convert_first("| A |\n| --- |\n| **bold** [link](url) |", Source::User).unwrap();
        let MarkdownNode::Table(table) = node else {
            unreachable!()
        };
        assert_eq!(
            table.rows[1].cells[0].children,
            vec![MarkdownNode::Text {
                value: "bold link".to_owned()
            }]
        );
    }

    #[test]
    fn definition_skipped() {
        let text = "[label]: https://example.com\n\nSome text";
        let children = root_children(text);
        // First child is Definition → should be None
        assert!(convert_node(&children[0], Source::Assistant).is_none());
        // Second child is Paragraph → should convert
        assert!(convert_node(&children[1], Source::Assistant).is_some());
    }

    // ── Inline conversions (Assistant) ─────────────────────────────

    #[test]
    fn strong_inline() {
        let node = convert_first("**bold**", Source::Assistant).unwrap();
        let MarkdownNode::Paragraph { children } = node else {
            unreachable!()
        };
        assert!(matches!(children[0], MarkdownNode::Strong { .. }));
    }

    #[test]
    fn emphasis_inline() {
        let node = convert_first("*italic*", Source::Assistant).unwrap();
        let MarkdownNode::Paragraph { children } = node else {
            unreachable!()
        };
        assert!(matches!(children[0], MarkdownNode::Emphasis { .. }));
    }

    #[test]
    fn inline_code() {
        let node = convert_first("`code`", Source::Assistant).unwrap();
        let MarkdownNode::Paragraph { children } = node else {
            unreachable!()
        };
        assert_eq!(
            children[0],
            MarkdownNode::InlineCode {
                value: "code".to_owned()
            }
        );
    }

    #[test]
    fn link_inline() {
        let node = convert_first("[text](https://example.com)", Source::Assistant).unwrap();
        let MarkdownNode::Paragraph { children } = node else {
            unreachable!()
        };
        assert!(matches!(
            &children[0],
            MarkdownNode::Link { url, .. } if url == "https://example.com"
        ));
    }

    #[test]
    fn image_inline() {
        let node = convert_first("![alt](img.png)", Source::Assistant).unwrap();
        let MarkdownNode::Paragraph { children } = node else {
            unreachable!()
        };
        assert!(matches!(
            &children[0],
            MarkdownNode::Image { url, alt, .. } if url == "img.png" && alt == "alt"
        ));
    }

    #[test]
    fn break_inline() {
        let node = convert_first("line1\\\nline2", Source::Assistant).unwrap();
        let MarkdownNode::Paragraph { children } = node else {
            unreachable!()
        };
        assert!(children.iter().any(|c| matches!(c, MarkdownNode::Break)));
    }

    // ── Source::User flattening ────────────────────────────────────

    #[test]
    fn user_source_flattens_strong_to_text() {
        let node = convert_first("**bold** text", Source::User).unwrap();
        let MarkdownNode::Paragraph { children } = node else {
            unreachable!()
        };
        // Should be a single Text node with no styling
        assert_eq!(children.len(), 1);
        assert_eq!(
            children[0],
            MarkdownNode::Text {
                value: "bold text".to_owned()
            }
        );
    }

    #[test]
    fn user_source_flattens_emphasis_to_text() {
        let node = convert_first("*italic*", Source::User).unwrap();
        let MarkdownNode::Paragraph { children } = node else {
            unreachable!()
        };
        assert_eq!(children.len(), 1);
        assert_eq!(
            children[0],
            MarkdownNode::Text {
                value: "italic".to_owned()
            }
        );
    }

    #[test]
    fn user_source_flattens_inline_code_to_text() {
        let node = convert_first("`code`", Source::User).unwrap();
        let MarkdownNode::Paragraph { children } = node else {
            unreachable!()
        };
        assert_eq!(children.len(), 1);
        assert_eq!(
            children[0],
            MarkdownNode::Text {
                value: "code".to_owned()
            }
        );
    }

    #[test]
    fn user_source_flattens_link_to_text() {
        let node = convert_first("[click](url)", Source::User).unwrap();
        let MarkdownNode::Paragraph { children } = node else {
            unreachable!()
        };
        assert_eq!(children.len(), 1);
        assert_eq!(
            children[0],
            MarkdownNode::Text {
                value: "click".to_owned()
            }
        );
    }

    #[test]
    fn user_source_preserves_block_structure() {
        // User source should still have headings, lists, blockquotes
        let heading = convert_first("# Title", Source::User).unwrap();
        assert!(matches!(heading, MarkdownNode::Heading { .. }));

        let list = convert_first("- item", Source::User).unwrap();
        assert!(matches!(list, MarkdownNode::List { .. }));

        let bq = convert_first("> quote", Source::User).unwrap();
        assert!(matches!(bq, MarkdownNode::Blockquote { .. }));
    }

    #[test]
    fn user_source_heading_inline_flattened() {
        let node = convert_first("# **Bold** title", Source::User).unwrap();
        let MarkdownNode::Heading { children, depth } = node else {
            unreachable!()
        };
        assert_eq!(depth, 1);
        // Inline children should be flattened to plain text
        assert_eq!(children.len(), 1);
        assert_eq!(
            children[0],
            MarkdownNode::Text {
                value: "Bold title".to_owned()
            }
        );
    }

    #[test]
    fn user_source_inline_styles_render_as_default_text() {
        let node = convert_first("**bold** *italic* `code` [link](url)", Source::User).unwrap();
        let buf = render_node(node, 80, 1);

        assert_plain_style(rendered_style_for_text(&buf, 0, "bold"));
        assert_plain_style(rendered_style_for_text(&buf, 0, "italic"));
        assert_plain_style(rendered_style_for_text(&buf, 0, "code"));
        assert_plain_style(rendered_style_for_text(&buf, 0, "link"));
    }

    // ── Nested structures ──────────────────────────────────────────

    #[test]
    fn nested_blockquote() {
        let node = convert_first("> > nested", Source::Assistant).unwrap();
        let MarkdownNode::Blockquote { children } = node else {
            unreachable!()
        };
        let MarkdownNode::Blockquote { children: inner } = &children[0] else {
            unreachable!()
        };
        assert!(matches!(inner[0], MarkdownNode::Paragraph { .. }));
    }

    #[test]
    fn list_with_nested_paragraphs() {
        let node = convert_first("- para1\n\n  para2", Source::Assistant).unwrap();
        let MarkdownNode::List { children, .. } = node else {
            unreachable!()
        };
        let MarkdownNode::ListItem {
            children: li_children,
        } = &children[0]
        else {
            unreachable!()
        };
        // ListItem has two paragraph children (spread)
        assert!(!li_children.is_empty());
    }

    #[test]
    fn heading_with_mixed_inline() {
        let node = convert_first("# Hello **world** and `code`", Source::Assistant).unwrap();
        let MarkdownNode::Heading { children, .. } = node else {
            unreachable!()
        };
        // Should contain Text, Strong, Text, InlineCode, Text
        assert!(
            children
                .iter()
                .any(|c| matches!(c, MarkdownNode::Strong { .. }))
        );
        assert!(
            children
                .iter()
                .any(|c| matches!(c, MarkdownNode::InlineCode { .. }))
        );
    }

    #[test]
    fn link_with_inline_children() {
        let node = convert_first("[**bold** link](url)", Source::Assistant).unwrap();
        let MarkdownNode::Paragraph { children } = node else {
            unreachable!()
        };
        let MarkdownNode::Link { children, url, .. } = &children[0] else {
            unreachable!()
        };
        assert_eq!(url, "url");
        assert!(matches!(children[0], MarkdownNode::Strong { .. }));
    }
}
