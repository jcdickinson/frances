//! `MarkdownBlock` — a leaf [`Block`] that renders a single [`MarkdownNode`].
//!
//! Each block-level node from the mdast conversion is wrapped in a
//! `MarkdownBlock`. Container nodes (blockquote, list, list-item) manage
//! their own recursive rendering internally; from the container's
//! perspective every `MarkdownBlock` is a leaf (`parts()` → `&[]`).

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use frances_tui::block::{Block, BlockMeasureContext, BlockRenderContext, Sigil};
use frances_tui::widget::{EventContext, EventOutcome, Input, Theme};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::markdown_node::{MarkdownNode, MarkdownTable, TableAlignment, TableCell, TableRow};

// ── Public type ─────────────────────────────────────────────────────

/// A leaf block rendering a single [`MarkdownNode`].
#[derive(Debug, Clone)]
pub struct MarkdownBlock {
    pub node: MarkdownNode,
    scroll_x: Cell<usize>,
    trailing_blank: bool,
}

impl MarkdownBlock {
    pub fn new(node: MarkdownNode) -> Self {
        Self {
            node,
            scroll_x: Cell::new(0),
            trailing_blank: false,
        }
    }

    pub fn with_trailing_blank(mut self) -> Self {
        self.trailing_blank = true;
        self
    }
}

impl Input for MarkdownBlock {
    fn handle_event(
        &mut self,
        _ctx: &mut EventContext<'_>,
        event: &crossterm::event::Event,
    ) -> EventOutcome {
        if !matches!(self.node, MarkdownNode::Table(_)) {
            return EventOutcome::Pass;
        }
        let crossterm::event::Event::Key(key) = event else {
            return EventOutcome::Pass;
        };
        if key.kind != KeyEventKind::Press {
            return EventOutcome::Pass;
        }

        match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                self.scroll_x.set(self.scroll_x.get().saturating_sub(1));
                EventOutcome::Consumed
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.scroll_x.set(self.scroll_x.get().saturating_add(1));
                EventOutcome::Consumed
            }
            KeyCode::Char('H') => {
                self.scroll_x.set(0);
                EventOutcome::Consumed
            }
            KeyCode::Char('L') => {
                self.scroll_x.set(usize::MAX);
                EventOutcome::Consumed
            }
            _ => EventOutcome::Pass,
        }
    }
}

impl Block for MarkdownBlock {
    fn safe_on_push(&self) -> bool {
        true
    }

    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        measure_node(&self.node, ctx.width, ctx.selected, ctx.theme)
            .saturating_add(self.trailing_blank as u16)
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil {
        render_node(
            &self.node,
            ctx.area,
            ctx.buf,
            ctx.src_y,
            ctx.alt_view && ctx.selected,
            &self.scroll_x,
            ctx.theme,
        );
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

        MarkdownNode::Table(table) => build_table_paragraph(table),

        // ThematicBreak is handled specially in `render_node`; this is a
        // fallback that should never be reached.
        _ => Paragraph::new(Text::from(Line::from(Span::styled(
            String::new(),
            Style::default(),
        )))),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Table builder
// ═══════════════════════════════════════════════════════════════════════

fn build_table_paragraph(table: &MarkdownTable) -> Paragraph<'static> {
    let lines = render_table_lines(table, TableCellBreaks::Spaces);
    Paragraph::new(Text::from(lines))
}

#[derive(Debug, Clone, Copy)]
enum TableCellBreaks {
    Spaces,
    Lines,
}

fn render_table_lines(table: &MarkdownTable, breaks: TableCellBreaks) -> Vec<Line<'static>> {
    if table.rows.is_empty() {
        return vec![Line::from(Span::styled(String::new(), Style::default()))];
    }

    let columns = table_column_count(table);
    if columns == 0 {
        return vec![Line::from(Span::styled(String::new(), Style::default()))];
    }

    let rendered_rows: Vec<Vec<Vec<Vec<Span<'static>>>>> = table
        .rows
        .iter()
        .map(|row| render_table_row_cells(row, columns, breaks))
        .collect();
    let widths = table_column_widths(&rendered_rows, columns);

    let mut lines = Vec::new();
    lines.extend(render_table_row_lines(
        &rendered_rows[0],
        &widths,
        &table.alignments,
    ));
    lines.push(render_table_separator(&widths, &table.alignments));

    for row in rendered_rows.iter().skip(1) {
        lines.extend(render_table_row_lines(row, &widths, &table.alignments));
    }

    lines
}

fn table_column_count(table: &MarkdownTable) -> usize {
    table
        .rows
        .iter()
        .map(|row| row.cells.len())
        .chain(std::iter::once(table.alignments.len()))
        .max()
        .unwrap_or(0)
}

fn render_table_row_cells(
    row: &TableRow,
    columns: usize,
    breaks: TableCellBreaks,
) -> Vec<Vec<Vec<Span<'static>>>> {
    (0..columns)
        .map(|column| {
            row.cells
                .get(column)
                .map(|cell| render_table_cell_lines(cell, breaks))
                .unwrap_or_else(|| vec![Vec::new()])
        })
        .collect()
}

fn render_table_cell_lines(cell: &TableCell, breaks: TableCellBreaks) -> Vec<Vec<Span<'static>>> {
    let mut lines = vec![Vec::new()];
    for child in &cell.children {
        match child {
            MarkdownNode::Break => match breaks {
                TableCellBreaks::Spaces => lines
                    .last_mut()
                    .expect("table cell lines are always non-empty")
                    .push(Span::styled(" ".to_string(), Style::default())),
                TableCellBreaks::Lines => lines.push(Vec::new()),
            },
            other => collect_inline_spans(
                other,
                lines
                    .last_mut()
                    .expect("table cell lines are always non-empty"),
                Style::default(),
            ),
        }
    }
    lines
}

fn table_column_widths(rows: &[Vec<Vec<Vec<Span<'static>>>>], columns: usize) -> Vec<usize> {
    let mut widths = vec![3; columns];
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            for line in cell {
                widths[column] = widths[column].max(spans_width(line));
            }
        }
    }
    widths
}

fn render_table_row_lines(
    cells: &[Vec<Vec<Span<'static>>>],
    widths: &[usize],
    alignments: &[TableAlignment],
) -> Vec<Line<'static>> {
    let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
    (0..height)
        .map(|line_index| render_table_row_line(cells, widths, alignments, line_index))
        .collect()
}

fn render_table_row_line(
    cells: &[Vec<Vec<Span<'static>>>],
    widths: &[usize],
    alignments: &[TableAlignment],
    line_index: usize,
) -> Line<'static> {
    let mut spans = vec![Span::styled("|".to_string(), Style::default())];

    for (column, width) in widths.iter().enumerate() {
        let cell_line = cells
            .get(column)
            .and_then(|cell| cell.get(line_index))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let alignment = table_alignment(alignments, column);
        spans.push(Span::styled(" ".to_string(), Style::default()));
        spans.extend(pad_table_cell(cell_line, *width, alignment));
        spans.push(Span::styled(" |".to_string(), Style::default()));
    }

    Line::from(spans)
}

fn render_table_separator(widths: &[usize], alignments: &[TableAlignment]) -> Line<'static> {
    let mut spans = vec![Span::styled("|".to_string(), Style::default())];

    for (column, width) in widths.iter().enumerate() {
        spans.push(Span::styled(" ".to_string(), Style::default()));
        spans.push(Span::styled(
            table_separator_cell(*width, table_alignment(alignments, column)),
            Style::default(),
        ));
        spans.push(Span::styled(" |".to_string(), Style::default()));
    }

    Line::from(spans)
}

fn table_alignment(alignments: &[TableAlignment], column: usize) -> TableAlignment {
    alignments
        .get(column)
        .copied()
        .unwrap_or(TableAlignment::None)
}

fn table_separator_cell(width: usize, alignment: TableAlignment) -> String {
    let width = width.max(3);
    match alignment {
        TableAlignment::None => "-".repeat(width),
        TableAlignment::Left => format!(":{}", "-".repeat(width - 1)),
        TableAlignment::Right => format!("{}:", "-".repeat(width - 1)),
        TableAlignment::Center => format!(":{}:", "-".repeat(width - 2)),
    }
}

fn pad_table_cell(
    cell: &[Span<'static>],
    width: usize,
    alignment: TableAlignment,
) -> Vec<Span<'static>> {
    let content_width = spans_width(cell);
    let padding = width.saturating_sub(content_width);
    let (left, right) = match alignment {
        TableAlignment::Right => (padding, 0),
        TableAlignment::Center => (padding / 2, padding - padding / 2),
        TableAlignment::None | TableAlignment::Left => (0, padding),
    };

    let mut spans = Vec::new();
    if left > 0 {
        spans.push(Span::styled(" ".repeat(left), Style::default()));
    }
    spans.extend(cell.iter().cloned());
    if right > 0 {
        spans.push(Span::styled(" ".repeat(right), Style::default()));
    }
    spans
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn line_width(line: &Line<'static>) -> usize {
    spans_width(&line.spans)
}

fn max_line_width(lines: &[Line<'static>]) -> usize {
    lines.iter().map(line_width).max().unwrap_or(0)
}

fn slice_table_lines(
    lines: &[Line<'static>],
    scroll_x: &Cell<usize>,
    visible_width: u16,
) -> Vec<Line<'static>> {
    let visible_width = visible_width as usize;
    let max_scroll = max_line_width(lines).saturating_sub(visible_width);
    let start = scroll_x.get().min(max_scroll);
    scroll_x.set(start);
    lines
        .iter()
        .map(|line| slice_line(line, start, visible_width))
        .collect()
}

fn slice_line(line: &Line<'static>, start: usize, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::from(Vec::<Span<'static>>::new());
    }

    let end = start.saturating_add(width);
    let mut cursor: usize = 0;
    let mut out = Vec::new();

    for span in &line.spans {
        let mut text = String::new();
        for ch in span.content.chars() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            let char_start = cursor;
            let char_end = cursor.saturating_add(char_width);
            if char_width == 0 {
                if char_start >= start && char_start < end {
                    text.push(ch);
                }
            } else if char_start >= start && char_end <= end {
                text.push(ch);
            }
            cursor = char_end;
            if cursor >= end {
                break;
            }
        }
        if !text.is_empty() {
            out.push(Span::styled(text, span.style));
        }
        if cursor >= end {
            break;
        }
    }

    Line::from(out)
}

// ═══════════════════════════════════════════════════════════════════════
// Measurement
// ═══════════════════════════════════════════════════════════════════════

fn measure_node(node: &MarkdownNode, width: u16, selected: bool, theme: &Theme) -> u16 {
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
        MarkdownNode::Table(table) => {
            let breaks = if selected {
                TableCellBreaks::Lines
            } else {
                TableCellBreaks::Spaces
            };
            render_table_lines(table, breaks).len() as u16
        }

        // Container nodes — recursive sum
        MarkdownNode::Blockquote { children } => {
            let inner = width.saturating_sub(2).max(1);
            children
                .iter()
                .map(|c| measure_node(c, inner, selected, theme))
                .fold(0u16, u16::saturating_add)
        }

        MarkdownNode::List {
            ordered,
            start,
            children,
        } => measure_list(
            *ordered,
            start.unwrap_or(1),
            children,
            width,
            selected,
            theme,
        ),

        MarkdownNode::ListItem { children } => children
            .iter()
            .map(|c| measure_node(c, width, selected, theme))
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
    selected: bool,
    theme: &Theme,
) -> u16 {
    let mut total: u16 = 0;
    for (i, item) in items.iter().enumerate() {
        let marker = list_marker(ordered, start_num + i as u32);
        let mw = marker.len() as u16;
        let inner = width.saturating_sub(mw).max(1);
        total = total.saturating_add(list_item_height(item, inner, selected, theme));
    }
    total
}

/// Height of a single list item (at least 1 row).
fn list_item_height(item: &MarkdownNode, inner_width: u16, selected: bool, theme: &Theme) -> u16 {
    match item {
        MarkdownNode::ListItem { children } => children
            .iter()
            .map(|c| measure_node(c, inner_width, selected, theme))
            .fold(0u16, u16::saturating_add)
            .max(1),
        _ => measure_node(item, inner_width, selected, theme).max(1),
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

fn render_node(
    node: &MarkdownNode,
    area: Rect,
    buf: &mut Buffer,
    src_y: u16,
    table_selected: bool,
    scroll_x: &Cell<usize>,
    theme: &Theme,
) {
    match node {
        MarkdownNode::Paragraph { .. }
        | MarkdownNode::Heading { .. }
        | MarkdownNode::Code { .. }
        | MarkdownNode::Html { .. } => {
            let para = build_leaf_paragraph(node);
            render_shifted_paragraph(&para, area, buf, src_y);
        }

        MarkdownNode::Table(table) if table_selected => {
            let lines = render_table_lines(table, TableCellBreaks::Lines);
            let lines = slice_table_lines(&lines, scroll_x, area.width);
            let para = Paragraph::new(Text::from(lines));
            render_shifted_paragraph(&para, area, buf, src_y);
        }

        MarkdownNode::Table(_) => {
            let para = build_leaf_paragraph(node);
            render_shifted_paragraph(&para, area, buf, src_y);
        }

        MarkdownNode::ThematicBreak if src_y == 0 => {
            let bar: String = "─".repeat(area.width as usize);
            buf.set_string(area.x, area.y, bar, theme.dim);
        }

        MarkdownNode::ThematicBreak => {}

        MarkdownNode::Blockquote { children } => {
            render_blockquote(children, area, buf, src_y, table_selected, scroll_x, theme);
        }

        MarkdownNode::List {
            ordered,
            start,
            children,
        } => {
            let list = ListRender {
                ordered: *ordered,
                start_num: start.unwrap_or(1),
                items: children,
            };
            render_list(list, area, buf, src_y, table_selected, scroll_x, theme);
        }

        MarkdownNode::ListItem { children } => {
            render_children_vertically(children, area, buf, src_y, table_selected, scroll_x, theme);
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
    table_selected: bool,
    scroll_x: &Cell<usize>,
    theme: &Theme,
) {
    let width = area.width.max(1);
    let mut cum: u16 = 0;
    for child in children {
        let h = measure_node(child, width, table_selected, theme);
        let child_src_y = src_y.saturating_sub(cum);
        if child_src_y < h {
            let top = cum.saturating_sub(src_y);
            let vis = h
                .saturating_sub(child_src_y)
                .min(area.height.saturating_sub(top));
            if vis > 0 {
                let child_area = Rect::new(area.x, area.y + top, area.width, vis);
                render_node(
                    child,
                    child_area,
                    buf,
                    child_src_y,
                    table_selected,
                    scroll_x,
                    theme,
                );
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
    table_selected: bool,
    scroll_x: &Cell<usize>,
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
        let h = measure_node(child, inner_width, table_selected, theme);
        let child_src_y = src_y.saturating_sub(cum);
        if child_src_y < h {
            let top = cum.saturating_sub(src_y);
            let vis = h
                .saturating_sub(child_src_y)
                .min(area.height.saturating_sub(top));
            if vis > 0 {
                let child_area = Rect::new(area.x + 2, area.y + top, inner_width, vis);
                render_node(
                    child,
                    child_area,
                    buf,
                    child_src_y,
                    table_selected,
                    scroll_x,
                    theme,
                );
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
struct ListRender<'a> {
    ordered: bool,
    start_num: u32,
    items: &'a [MarkdownNode],
}

fn render_list(
    list: ListRender<'_>,
    area: Rect,
    buf: &mut Buffer,
    src_y: u16,
    table_selected: bool,
    scroll_x: &Cell<usize>,
    theme: &Theme,
) {
    let mut cum: u16 = 0;
    for (i, item) in list.items.iter().enumerate() {
        let marker = list_marker(list.ordered, list.start_num + i as u32);
        let mw = marker.len() as u16;
        let inner_width = area.width.saturating_sub(mw);
        if inner_width == 0 {
            continue;
        }
        let h = list_item_height(item, inner_width, table_selected, theme);
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
                        render_children_vertically(
                            children,
                            child_area,
                            buf,
                            item_src_y,
                            table_selected,
                            scroll_x,
                            theme,
                        );
                    }
                    _ => render_node(
                        item,
                        child_area,
                        buf,
                        item_src_y,
                        table_selected,
                        scroll_x,
                        theme,
                    ),
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

    use crate::markdown_node::{MarkdownTable, TableCell, TableRow};

    /// Helper: wrap plain text in a Paragraph (matches real mdast structure
    /// where list-item children are block-level nodes like Paragraph).
    fn li_para(text: &str) -> MarkdownNode {
        MarkdownNode::Paragraph {
            children: vec![MarkdownNode::Text { value: text.into() }],
        }
    }

    fn table_cell(children: Vec<MarkdownNode>) -> TableCell {
        TableCell { children }
    }

    fn table_text(value: &str) -> TableCell {
        table_cell(vec![MarkdownNode::Text {
            value: value.to_string(),
        }])
    }

    fn table_node(alignments: Vec<TableAlignment>, rows: Vec<Vec<TableCell>>) -> MarkdownNode {
        MarkdownNode::Table(MarkdownTable {
            alignments,
            rows: rows.into_iter().map(|cells| TableRow { cells }).collect(),
        })
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

    fn measure_selected_at(node: MarkdownNode, width: u16) -> u16 {
        let block = MarkdownBlock::new(node);
        let ctx = BlockMeasureContext {
            width,
            selected: true,
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

    fn render_block(
        block: &MarkdownBlock,
        width: u16,
        height: u16,
        alt_view: bool,
        selected: bool,
    ) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        let ft = StubFrameTime;
        let mut render_ctx = BlockRenderContext {
            area,
            buf: &mut buf,
            src_y: 0,
            truncated: false,
            alt_view,
            selected,
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
    fn measure_table_counts_header_separator_and_body_rows() {
        let node = table_node(
            vec![TableAlignment::None, TableAlignment::None],
            vec![
                vec![table_text("Name"), table_text("Value")],
                vec![table_text("alpha"), table_text("1")],
                vec![table_text("beta"), table_text("2")],
            ],
        );

        assert_eq!(measure_at(node, 80), 4);
    }

    #[test]
    fn render_table_as_source_like_pipe_rows() {
        let node = table_node(
            vec![TableAlignment::Left, TableAlignment::Right],
            vec![
                vec![table_text("Name"), table_text("Count")],
                vec![table_text("alpha"), table_text("12")],
            ],
        );
        let buf = render_at(node, 80, 3);

        assert_eq!(row_text(&buf, 0), "| Name  | Count |");
        assert_eq!(row_text(&buf, 1), "| :---- | ----: |");
        assert_eq!(row_text(&buf, 2), "| alpha |    12 |");
    }

    #[test]
    fn render_table_center_alignment_padding() {
        let node = table_node(
            vec![TableAlignment::Center],
            vec![vec![table_text("Header")], vec![table_text("x")]],
        );
        let buf = render_at(node, 80, 3);

        assert_eq!(row_text(&buf, 1), "| :----: |");
        assert_eq!(row_text(&buf, 2), "|   x    |");
    }

    #[test]
    fn render_table_replaces_hard_breaks_with_spaces() {
        let node = table_node(
            vec![TableAlignment::None],
            vec![
                vec![table_text("Header")],
                vec![table_cell(vec![
                    MarkdownNode::Text {
                        value: "first".into(),
                    },
                    MarkdownNode::Break,
                    MarkdownNode::Text {
                        value: "second".into(),
                    },
                ])],
            ],
        );
        let buf = render_at(node, 80, 3);

        assert_eq!(row_text(&buf, 2), "| first second |");
    }

    #[test]
    fn render_table_clips_to_area_width() {
        let node = table_node(
            vec![TableAlignment::None],
            vec![vec![table_text("Header")], vec![table_text("long value")]],
        );
        let buf = render_at(node, 8, 3);

        assert_eq!(row_text(&buf, 2), "| long v");
    }

    #[test]
    fn selected_table_measures_hard_breaks_as_multiline_rows() {
        let node = table_node(
            vec![TableAlignment::None],
            vec![
                vec![table_text("Header")],
                vec![table_cell(vec![
                    MarkdownNode::Text {
                        value: "first".into(),
                    },
                    MarkdownNode::Break,
                    MarkdownNode::Text {
                        value: "second".into(),
                    },
                ])],
            ],
        );

        assert_eq!(measure_at(node.clone(), 80), 3);
        assert_eq!(measure_selected_at(node, 80), 4);
    }

    #[test]
    fn selected_alt_table_renders_hard_breaks_as_continuation_rows() {
        let node = table_node(
            vec![TableAlignment::None, TableAlignment::None],
            vec![
                vec![table_text("Name"), table_text("Notes")],
                vec![
                    table_text("alpha"),
                    table_cell(vec![
                        MarkdownNode::Text {
                            value: "first".into(),
                        },
                        MarkdownNode::Break,
                        MarkdownNode::Text {
                            value: "second".into(),
                        },
                    ]),
                ],
            ],
        );
        let block = MarkdownBlock::new(node);
        let buf = render_block(&block, 80, 4, true, true);

        assert_eq!(row_text(&buf, 2), "| alpha | first  |");
        assert_eq!(row_text(&buf, 3), "|       | second |");
    }

    #[test]
    fn selected_alt_table_scrolls_horizontally() {
        let node = table_node(
            vec![TableAlignment::None],
            vec![vec![table_text("Header")], vec![table_text("abcdef")]],
        );
        let mut block = MarkdownBlock::new(node);

        let before = render_block(&block, 8, 3, true, true);
        assert_eq!(row_text(&before, 2), "| abcdef");

        let mut redraw = false;
        let mut focus = frances_tui::widget::Focus::default();
        let mut ctx = EventContext {
            focus: &mut focus,
            redraw: &mut redraw,
        };
        let event = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('l'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(matches!(
            block.handle_event(&mut ctx, &event),
            EventOutcome::Consumed
        ));

        let after = render_block(&block, 8, 3, true, true);
        assert_eq!(row_text(&after, 2), " abcdef");
    }

    #[test]
    fn selected_alt_table_big_l_scrolls_to_right_edge_and_clamps() {
        let node = table_node(
            vec![TableAlignment::None],
            vec![vec![table_text("Header")], vec![table_text("abcdef")]],
        );
        let mut block = MarkdownBlock::new(node);
        let mut redraw = false;
        let mut focus = frances_tui::widget::Focus::default();
        let mut ctx = EventContext {
            focus: &mut focus,
            redraw: &mut redraw,
        };
        let event = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('L'),
            crossterm::event::KeyModifiers::NONE,
        ));

        assert!(matches!(
            block.handle_event(&mut ctx, &event),
            EventOutcome::Consumed
        ));
        let buf = render_block(&block, 8, 3, true, true);

        assert_eq!(row_text(&buf, 2), "abcdef |");
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
