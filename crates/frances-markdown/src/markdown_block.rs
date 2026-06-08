//! `MarkdownBlock` — a leaf [`Block`] that renders a single [`MarkdownNode`].
//!
//! Each block-level node from the mdast conversion is wrapped in a
//! `MarkdownBlock`. Container nodes (blockquote, list, list-item) manage
//! their own recursive rendering internally; from the container's
//! perspective every `MarkdownBlock` is a leaf (`parts()` → `&[]`).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use frances_tui::block::{Block, BlockMeasureContext, BlockRenderContext, Sigil};
use frances_tui::widget::{EventContext, EventOutcome, Input, Theme};

use crate::markdown_node::MarkdownNode;

// ── Public type ─────────────────────────────────────────────────────

/// A leaf block rendering a single [`MarkdownNode`].
#[derive(Debug, Clone)]
pub struct MarkdownBlock {
    pub node: MarkdownNode,
}

impl MarkdownBlock {
    pub fn new(node: MarkdownNode) -> Self {
        Self { node }
    }
}

impl Input for MarkdownBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        _event: &crossterm::event::Event,
    ) -> EventOutcome {
        EventOutcome::Pass
    }
}

impl Block for MarkdownBlock {
    fn safe_on_push(&self) -> bool {
        true
    }

    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        measure_node(&self.node, ctx.width, ctx.theme)
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil {
        render_node(&self.node, ctx.area, ctx.buf, ctx.src_y, ctx.theme);
        Sigil::blank()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Inline → Lines
// ═══════════════════════════════════════════════════════════════════════

/// Convert inline children into styled lines, splitting on [`MarkdownNode::Break`].
fn inline_to_lines(children: &[MarkdownNode]) -> Vec<Line<'static>> {
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut lines: Vec<Line<'static>> = Vec::new();

    for child in children {
        match child {
            MarkdownNode::Break => {
                lines.push(Line::from(std::mem::take(&mut current)));
            }
            other => collect_inline_spans(other, &mut current, Style::default()),
        }
    }

    if !current.is_empty() || lines.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

/// Recursively push styled spans for one inline node into `out`.
fn collect_inline_spans(node: &MarkdownNode, out: &mut Vec<Span<'static>>, base: Style) {
    match node {
        MarkdownNode::Text { value } => {
            out.push(Span::styled(value.clone(), base));
        }
        MarkdownNode::Strong { children } => {
            let s = base.add_modifier(Modifier::BOLD);
            for c in children {
                collect_inline_spans(c, out, s);
            }
        }
        MarkdownNode::Emphasis { children } => {
            let s = base.add_modifier(Modifier::ITALIC);
            for c in children {
                collect_inline_spans(c, out, s);
            }
        }
        MarkdownNode::InlineCode { value } => {
            out.push(Span::styled(value.clone(), base));
        }
        MarkdownNode::Link { url, children, .. } => {
            for c in children {
                collect_inline_spans(c, out, base);
            }
            out.push(Span::styled(format!(" ({url})"), base));
        }
        MarkdownNode::Image { alt, .. } => {
            out.push(Span::styled(format!("[{alt}]"), base));
        }
        MarkdownNode::Break => { /* handled by caller */ }
        _ => {}
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Leaf Paragraph builder
// ═══════════════════════════════════════════════════════════════════════

/// Build a [`Paragraph`] for a leaf-level node that ratatui can render
/// directly.  Paragraphs and headings use word-wrapping; code, HTML and
/// thematic breaks do not.
fn build_leaf_paragraph(node: &MarkdownNode) -> Paragraph<'static> {
    match node {
        MarkdownNode::Paragraph { children } => {
            let lines = inline_to_lines(children);
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false })
        }

        MarkdownNode::Heading { depth, children } => {
            let prefix = "#".repeat(*depth as usize) + " ";
            let mut lines = inline_to_lines(children);
            // Prepend heading prefix to the first line.
            if let Some(first) = lines.first_mut() {
                let mut prefixed = vec![Span::styled(
                    prefix,
                    Style::default().add_modifier(Modifier::BOLD),
                )];
                prefixed.append(&mut first.spans);
                first.spans = prefixed;
            }
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false })
        }

        MarkdownNode::Code { lang, value } => {
            let mut lines: Vec<Line<'static>> = Vec::new();
            if let Some(l) = lang {
                lines.push(Line::from(Span::styled(
                    l.clone(),
                    Style::default().add_modifier(Modifier::DIM),
                )));
            }
            for line in value.lines() {
                lines.push(Line::from(Span::styled(line.to_owned(), Style::default())));
            }
            if lines.is_empty() {
                lines.push(Line::from(Span::styled(String::new(), Style::default())));
            }
            // No wrapping — code lines are left as-is, clipped by the buffer.
            Paragraph::new(Text::from(lines))
        }

        MarkdownNode::Html { value } => {
            let lines: Vec<Line<'static>> = if value.is_empty() {
                vec![Line::from(Span::styled(String::new(), Style::default()))]
            } else {
                value
                    .lines()
                    .map(|l| Line::from(Span::styled(l.to_owned(), Style::default())))
                    .collect()
            };
            Paragraph::new(Text::from(lines))
        }

        // ThematicBreak is handled specially in `render_node`; this is a
        // fallback that should never be reached.
        _ => Paragraph::new(Text::from(Line::from(Span::styled(
            String::new(),
            Style::default(),
        )))),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Measurement
// ═══════════════════════════════════════════════════════════════════════

fn measure_node(node: &MarkdownNode, width: u16, theme: &Theme) -> u16 {
    let width = width.max(1);
    match node {
        // Wrapped leaf nodes
        MarkdownNode::Paragraph { .. } | MarkdownNode::Heading { .. } => {
            build_leaf_paragraph(node).line_count(width) as u16
        }

        // Unwrapped leaf nodes — raw line count
        MarkdownNode::Code { lang, value } => {
            let n = lang.is_some() as u16 + value.lines().count() as u16;
            n.max(1)
        }
        MarkdownNode::Html { value } => (value.lines().count() as u16).max(1),
        MarkdownNode::ThematicBreak => 1,

        // Container nodes — recursive sum
        MarkdownNode::Blockquote { children } => {
            let inner = width.saturating_sub(2).max(1);
            children
                .iter()
                .map(|c| measure_node(c, inner, theme))
                .fold(0u16, u16::saturating_add)
        }

        MarkdownNode::List {
            ordered,
            start,
            children,
        } => measure_list(*ordered, start.unwrap_or(1), children, width, theme),

        MarkdownNode::ListItem { children } => children
            .iter()
            .map(|c| measure_node(c, width, theme))
            .fold(0u16, u16::saturating_add)
            .max(1),

        // Inline at root level (defensive)
        _ => 1,
    }
}

fn measure_list(
    ordered: bool,
    start_num: u32,
    items: &[MarkdownNode],
    width: u16,
    theme: &Theme,
) -> u16 {
    let mut total: u16 = 0;
    for (i, item) in items.iter().enumerate() {
        let marker = list_marker(ordered, start_num + i as u32);
        let mw = marker.len() as u16;
        let inner = width.saturating_sub(mw).max(1);
        total = total.saturating_add(list_item_height(item, inner, theme));
    }
    total
}

/// Height of a single list item (at least 1 row).
fn list_item_height(item: &MarkdownNode, inner_width: u16, theme: &Theme) -> u16 {
    match item {
        MarkdownNode::ListItem { children } => children
            .iter()
            .map(|c| measure_node(c, inner_width, theme))
            .fold(0u16, u16::saturating_add)
            .max(1),
        _ => measure_node(item, inner_width, theme).max(1),
    }
}

fn list_marker(ordered: bool, num: u32) -> String {
    if ordered {
        format!("{num}. ")
    } else {
        "• ".to_string()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Rendering
// ═══════════════════════════════════════════════════════════════════════

fn render_node(node: &MarkdownNode, area: Rect, buf: &mut Buffer, src_y: u16, theme: &Theme) {
    match node {
        MarkdownNode::Paragraph { .. }
        | MarkdownNode::Heading { .. }
        | MarkdownNode::Code { .. }
        | MarkdownNode::Html { .. } => {
            let para = build_leaf_paragraph(node);
            render_shifted_paragraph(&para, area, buf, src_y);
        }

        MarkdownNode::ThematicBreak if src_y == 0 => {
            let bar: String = "─".repeat(area.width as usize);
            buf.set_string(area.x, area.y, bar, theme.dim);
        }

        MarkdownNode::ThematicBreak => {}

        MarkdownNode::Blockquote { children } => {
            render_blockquote(children, area, buf, src_y, theme);
        }

        MarkdownNode::List {
            ordered,
            start,
            children,
        } => {
            render_list(
                *ordered,
                start.unwrap_or(1),
                children,
                area,
                buf,
                src_y,
                theme,
            );
        }

        MarkdownNode::ListItem { children } => {
            render_children_vertically(children, area, buf, src_y, theme);
        }

        _ => {}
    }
}

/// Render a [`Paragraph`] with the src_y shift trick — paint into a
/// taller virtual rect above the visible area so the buffer clips the
/// top rows.
fn render_shifted_paragraph(para: &Paragraph<'static>, area: Rect, buf: &mut Buffer, src_y: u16) {
    let shifted = Rect::new(
        area.x,
        area.y.saturating_sub(src_y),
        area.width,
        area.height.saturating_add(src_y),
    );
    Widget::render(para.clone(), shifted, buf);
}

/// Render children stacked vertically, honouring `src_y`.
fn render_children_vertically(
    children: &[MarkdownNode],
    area: Rect,
    buf: &mut Buffer,
    src_y: u16,
    theme: &Theme,
) {
    let width = area.width.max(1);
    let mut cum: u16 = 0;
    for child in children {
        let h = measure_node(child, width, theme);
        let child_src_y = src_y.saturating_sub(cum);
        if child_src_y < h {
            let top = cum.saturating_sub(src_y);
            let vis = h
                .saturating_sub(child_src_y)
                .min(area.height.saturating_sub(top));
            if vis > 0 {
                let child_area = Rect::new(area.x, area.y + top, area.width, vis);
                render_node(child, child_area, buf, child_src_y, theme);
            }
        }
        cum = cum.saturating_add(h);
        if cum >= src_y.saturating_add(area.height) {
            break;
        }
    }
}

/// Render a blockquote: `"> "` prefix on each visible row, children in
/// the narrower inner area (width − 2).
fn render_blockquote(
    children: &[MarkdownNode],
    area: Rect,
    buf: &mut Buffer,
    src_y: u16,
    theme: &Theme,
) {
    // Prefix every visible row.
    for y in area.top()..area.bottom() {
        buf.set_string(area.x, y, "> ", theme.dim);
    }

    let inner_width = area.width.saturating_sub(2);
    if inner_width == 0 {
        return;
    }

    let mut cum: u16 = 0;
    for child in children {
        let h = measure_node(child, inner_width, theme);
        let child_src_y = src_y.saturating_sub(cum);
        if child_src_y < h {
            let top = cum.saturating_sub(src_y);
            let vis = h
                .saturating_sub(child_src_y)
                .min(area.height.saturating_sub(top));
            if vis > 0 {
                let child_area = Rect::new(area.x + 2, area.y + top, inner_width, vis);
                render_node(child, child_area, buf, child_src_y, theme);
            }
        }
        cum = cum.saturating_add(h);
        if cum >= src_y.saturating_add(area.height) {
            break;
        }
    }
}

/// Render a list: marker per item, then item children in the remaining
/// inner area.
fn render_list(
    ordered: bool,
    start_num: u32,
    items: &[MarkdownNode],
    area: Rect,
    buf: &mut Buffer,
    src_y: u16,
    theme: &Theme,
) {
    let mut cum: u16 = 0;
    for (i, item) in items.iter().enumerate() {
        let marker = list_marker(ordered, start_num + i as u32);
        let mw = marker.len() as u16;
        let inner_width = area.width.saturating_sub(mw);
        if inner_width == 0 {
            continue;
        }
        let h = list_item_height(item, inner_width, theme);
        let item_src_y = src_y.saturating_sub(cum);

        if item_src_y < h {
            let top = cum.saturating_sub(src_y);
            let vis = h
                .saturating_sub(item_src_y)
                .min(area.height.saturating_sub(top));

            if vis > 0 {
                // Paint the marker on the item's first visible row.
                if item_src_y == 0 && top < area.height {
                    buf.set_string(area.x, area.y + top, &marker, Style::default());
                }

                let child_area = Rect::new(area.x + mw, area.y + top, inner_width, vis);
                match item {
                    MarkdownNode::ListItem { children } => {
                        render_children_vertically(children, child_area, buf, item_src_y, theme);
                    }
                    _ => render_node(item, child_area, buf, item_src_y, theme),
                }
            }
        }

        cum = cum.saturating_add(h);
        if cum >= src_y.saturating_add(area.height) {
            break;
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use frances_tui::widget::FrameTime;

    /// Helper: wrap plain text in a Paragraph (matches real mdast structure
    /// where list-item children are block-level nodes like Paragraph).
    fn li_para(text: &str) -> MarkdownNode {
        MarkdownNode::Paragraph {
            children: vec![MarkdownNode::Text { value: text.into() }],
        }
    }

    /// Stub clock for tests — frame index is always 0.
    struct StubFrameTime;
    impl FrameTime for StubFrameTime {
        fn get_frame(&self) -> f64 {
            0.0
        }
    }

    fn theme() -> Theme {
        Theme::default()
    }

    fn measure_at(node: MarkdownNode, width: u16) -> u16 {
        let block = MarkdownBlock::new(node);
        let ctx = BlockMeasureContext {
            width,
            selected: false,
            selected_part: None,
            theme: &theme(),
        };
        block.measure(&ctx)
    }

    fn render_at(node: MarkdownNode, width: u16, height: u16) -> Buffer {
        let block = MarkdownBlock::new(node);
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        let ft = StubFrameTime;
        let mut render_ctx = BlockRenderContext {
            area,
            buf: &mut buf,
            src_y: 0,
            truncated: false,
            alt_view: false,
            selected: false,
            selected_part: None,
            theme: &theme(),
            frame_time: &ft,
        };
        block.render(&mut render_ctx);
        buf
    }

    /// Collect the rendered content of a single buffer row, trimmed.
    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect::<Vec<_>>()
            .join("")
            .trim_end()
            .to_string()
    }

    // ── Measure tests ─────────────────────────────────────────────

    #[test]
    fn measure_paragraph_single_line() {
        let node = MarkdownNode::Paragraph {
            children: vec![MarkdownNode::Text {
                value: "Hello world".into(),
            }],
        };
        assert_eq!(measure_at(node, 80), 1);
    }

    #[test]
    fn measure_paragraph_wraps() {
        let long = "word ".repeat(50);
        let node = MarkdownNode::Paragraph {
            children: vec![MarkdownNode::Text { value: long }],
        };
        assert!(measure_at(node, 20) > 1);
    }

    #[test]
    fn measure_heading() {
        let node = MarkdownNode::Heading {
            depth: 2,
            children: vec![MarkdownNode::Text {
                value: "Title".into(),
            }],
        };
        assert_eq!(measure_at(node, 80), 1);
    }

    #[test]
    fn measure_code_block_with_lang() {
        let node = MarkdownNode::Code {
            lang: Some("rust".into()),
            value: "fn main() {}".into(),
        };
        assert_eq!(measure_at(node, 80), 2);
    }

    #[test]
    fn measure_code_block_without_lang() {
        let node = MarkdownNode::Code {
            lang: None,
            value: "hello".into(),
        };
        assert_eq!(measure_at(node, 80), 1);
    }

    #[test]
    fn measure_html() {
        let node = MarkdownNode::Html {
            value: "<div>\nhello\n</div>".into(),
        };
        assert_eq!(measure_at(node, 80), 3);
    }

    #[test]
    fn measure_thematic_break() {
        assert_eq!(measure_at(MarkdownNode::ThematicBreak, 80), 1);
    }

    #[test]
    fn measure_blockquote() {
        let node = MarkdownNode::Blockquote {
            children: vec![MarkdownNode::Paragraph {
                children: vec![MarkdownNode::Text {
                    value: "quoted".into(),
                }],
            }],
        };
        assert_eq!(measure_at(node, 80), 1);
    }

    #[test]
    fn measure_unordered_list() {
        let node = MarkdownNode::List {
            ordered: false,
            start: None,
            children: vec![
                MarkdownNode::ListItem {
                    children: vec![li_para("one")],
                },
                MarkdownNode::ListItem {
                    children: vec![li_para("two")],
                },
            ],
        };
        assert_eq!(measure_at(node, 80), 2);
    }

    #[test]
    fn measure_ordered_list() {
        let node = MarkdownNode::List {
            ordered: true,
            start: Some(1),
            children: vec![
                MarkdownNode::ListItem {
                    children: vec![li_para("first")],
                },
                MarkdownNode::ListItem {
                    children: vec![li_para("second")],
                },
            ],
        };
        assert_eq!(measure_at(node, 80), 2);
    }

    // ── Render tests ──────────────────────────────────────────────

    #[test]
    fn render_paragraph_text() {
        let node = MarkdownNode::Paragraph {
            children: vec![MarkdownNode::Text {
                value: "Hello world".into(),
            }],
        };
        let buf = render_at(node, 80, 1);
        assert!(row_text(&buf, 0).contains("Hello world"));
    }

    #[test]
    fn render_heading_prefix() {
        let node = MarkdownNode::Heading {
            depth: 2,
            children: vec![MarkdownNode::Text {
                value: "Title".into(),
            }],
        };
        let buf = render_at(node, 80, 1);
        let text = row_text(&buf, 0);
        assert!(text.starts_with("## "));
        assert!(text.contains("Title"));
    }

    #[test]
    fn render_code_body() {
        let node = MarkdownNode::Code {
            lang: Some("rust".into()),
            value: "fn main() {}".into(),
        };
        let buf = render_at(node, 80, 2);
        assert!(row_text(&buf, 0).contains("rust"));
        assert!(row_text(&buf, 1).contains("fn main()"));
    }

    #[test]
    fn render_html_as_code() {
        let node = MarkdownNode::Html {
            value: "<div>hi</div>".into(),
        };
        let buf = render_at(node, 80, 1);
        assert!(row_text(&buf, 0).contains("<div>"));
    }

    #[test]
    fn render_thematic_break() {
        let buf = render_at(MarkdownNode::ThematicBreak, 40, 1);
        let text = row_text(&buf, 0);
        assert!(text.contains("─"));
    }

    #[test]
    fn render_blockquote_prefix() {
        let node = MarkdownNode::Blockquote {
            children: vec![MarkdownNode::Paragraph {
                children: vec![MarkdownNode::Text {
                    value: "quoted text".into(),
                }],
            }],
        };
        let buf = render_at(node, 80, 1);
        let text = row_text(&buf, 0);
        assert!(text.starts_with("> "));
        assert!(text.contains("quoted text"));
    }

    #[test]
    fn render_unordered_list_marker() {
        let node = MarkdownNode::List {
            ordered: false,
            start: None,
            children: vec![MarkdownNode::ListItem {
                children: vec![li_para("item")],
            }],
        };
        let buf = render_at(node, 80, 1);
        let text = row_text(&buf, 0);
        assert!(text.contains("•"));
        assert!(text.contains("item"));
    }

    #[test]
    fn render_ordered_list_marker() {
        let node = MarkdownNode::List {
            ordered: true,
            start: Some(1),
            children: vec![MarkdownNode::ListItem {
                children: vec![li_para("first")],
            }],
        };
        let buf = render_at(node, 80, 1);
        let text = row_text(&buf, 0);
        assert!(text.starts_with("1."));
        assert!(text.contains("first"));
    }

    #[test]
    fn render_inline_strong() {
        let node = MarkdownNode::Paragraph {
            children: vec![
                MarkdownNode::Text {
                    value: "before ".into(),
                },
                MarkdownNode::Strong {
                    children: vec![MarkdownNode::Text {
                        value: "bold".into(),
                    }],
                },
                MarkdownNode::Text {
                    value: " after".into(),
                },
            ],
        };
        let buf = render_at(node, 80, 1);
        let text = row_text(&buf, 0);
        assert!(text.contains("before"));
        assert!(text.contains("bold"));
        assert!(text.contains("after"));
    }

    #[test]
    fn render_inline_link() {
        let node = MarkdownNode::Paragraph {
            children: vec![MarkdownNode::Link {
                url: "https://example.com".into(),
                title: None,
                children: vec![MarkdownNode::Text {
                    value: "click".into(),
                }],
            }],
        };
        let buf = render_at(node, 80, 1);
        let text = row_text(&buf, 0);
        assert!(text.contains("click"));
        assert!(text.contains("https://example.com"));
    }

    #[test]
    fn render_nested_blockquote() {
        let node = MarkdownNode::Blockquote {
            children: vec![MarkdownNode::Blockquote {
                children: vec![MarkdownNode::Paragraph {
                    children: vec![MarkdownNode::Text {
                        value: "deep".into(),
                    }],
                }],
            }],
        };
        let buf = render_at(node, 80, 1);
        let text = row_text(&buf, 0);
        assert!(text.starts_with("> >"));
        assert!(text.contains("deep"));
    }

    #[test]
    fn render_two_list_items() {
        let node = MarkdownNode::List {
            ordered: false,
            start: None,
            children: vec![
                MarkdownNode::ListItem {
                    children: vec![li_para("first")],
                },
                MarkdownNode::ListItem {
                    children: vec![li_para("second")],
                },
            ],
        };
        let buf = render_at(node, 80, 2);
        let row0 = row_text(&buf, 0);
        let row1 = row_text(&buf, 1);
        assert!(row0.contains("•"));
        assert!(row0.contains("first"));
        assert!(row1.contains("•"));
        assert!(row1.contains("second"));
    }

    #[test]
    fn render_ordered_list_with_start() {
        let node = MarkdownNode::List {
            ordered: true,
            start: Some(3),
            children: vec![
                MarkdownNode::ListItem {
                    children: vec![li_para("gamma")],
                },
                MarkdownNode::ListItem {
                    children: vec![li_para("delta")],
                },
            ],
        };
        let buf = render_at(node, 80, 2);
        assert!(row_text(&buf, 0).starts_with("3."));
        assert!(row_text(&buf, 1).starts_with("4."));
    }

    #[test]
    fn render_empty_code_block() {
        let node = MarkdownNode::Code {
            lang: None,
            value: String::new(),
        };
        assert_eq!(measure_at(node.clone(), 80), 1);
        let _buf = render_at(node, 80, 1);
    }

    #[test]
    fn render_inline_code_span() {
        let node = MarkdownNode::Paragraph {
            children: vec![MarkdownNode::InlineCode {
                value: "code".into(),
            }],
        };
        let buf = render_at(node, 80, 1);
        assert!(row_text(&buf, 0).contains("code"));
    }

    #[test]
    fn render_image_alt_text() {
        let node = MarkdownNode::Paragraph {
            children: vec![MarkdownNode::Image {
                url: "img.png".into(),
                alt: "a diagram".into(),
                title: None,
            }],
        };
        let buf = render_at(node, 80, 1);
        assert!(row_text(&buf, 0).contains("[a diagram]"));
    }
}
