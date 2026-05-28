//! Scratchpad for `ScrollbackContainer`.
//!
//! Mirrors `frances-tui-scratch` but exercises the new container path
//! instead of calling `BottomBackend::emit_above` directly. The
//! footer block reports its own height; the container measures it,
//! decides which history rows fit above, and paints. Rows that fall
//! off the top are flag-marked but (today) not yet emitted into
//! native scrollback — that primitive lands in a follow-up.
//!
//! Controls (live view):
//!   p          push a single-line history row (lands in `safe`, or
//!              queues at the back of `active` flagged ready-to-promote
//!              if older active blocks are still in flight)
//!   P          push a 3-line history row (same path as `p`)
//!   a          start an active block, append text, mark safe
//!   v          push a varsize active block (starts at 5 rows)
//!   V          shrink the varsize block by 1, cycling back to 5 at 1
//!   s          push a fake-shell block with 30 body lines (drive
//!              Phase D selection + j/k/u/d scroll in alt-view)
//!   ↑ / K      grow footer by 1 row
//!   ↓ / J      shrink footer by 1 row
//!   Ctrl-O     enter alt-view inspector
//!   q / Esc    quit
//!
//! Controls (alt-view inspector):
//!   Tab / S-Tab   move block selection (newer / older)
//!   j / k / u / d forwarded to the selected block
//!   ↑ / ↓         scroll inspector window
//!   Esc / Ctrl-O  back to live view
//!   q             quit

use std::io::{self, Write, stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
};
use crossterm::{QueueableCommand, cursor};
use ratatui::Terminal;
use ratatui::TerminalOptions;
use ratatui::Viewport;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Size;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets;
use ratatui::widgets::{Borders, Paragraph};

use frances_tui::block::Sigil;
use frances_tui::scrollback_container::DrawContext;
use frances_tui::widget::{EventContext, EventOutcome, Input};
use frances_tui::{
    AnimationGate, Block, BlockId, BlockKind, BlockMeasureContext, BlockRenderContext, Focus,
    ParaWidget, ScrollbackBackend, ScrollbackContainer, Theme, WallClockFrameTime,
};

const VARSIZE_MAX: u16 = 5;

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let result = run();
    let _ = stdout().queue(cursor::Show);
    let _ = stdout().flush();
    let _ = disable_raw_mode();
    println!();
    result
}

fn run() -> io::Result<()> {
    let (w, h) = size()?;
    let term_size = Size {
        width: w,
        height: h,
    };

    // Anchor the container at the cursor's current row so we start
    // below whatever the shell printed last. The first container.draw
    // will grow content_h to match its desired layout.
    let (_, cursor_row) = crossterm::cursor::position()?;
    let backend = ScrollbackBackend::new(CrosstermBackend::new(stdout()), term_size);
    {
        let mut out = stdout();
        out.queue(cursor::Hide)?;
        out.flush()?;
    }

    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;

    let mut footer_content: u16 = 2;
    let mut container = ScrollbackContainer::new(cursor_row);
    let theme = Theme::default();
    let frame_time = WallClockFrameTime::new();
    let animation = AnimationGate::new();
    let mut focus = Focus::new();
    // Declared but uninitialised: every loop iteration overwrites
    // `footer` before reading it. Rust's CFG accepts this.
    let mut footer: ParaWidget;
    let mut pushed: u32 = 0;
    // Most recently pushed varsize block — `V` cycles its height
    // through the `update_active` path so the footer-pin behaviour
    // around content shrinkage can be eyeballed against new pushes.
    let mut varsize: Option<(BlockId, u32, u16)> = None;

    loop {
        footer = (*footer_block(
            container.safe_count() as u32,
            container.active_count() as u32,
            container.committed_count(),
            footer_content,
        ))
        .into();
        let ctx = DrawContext {
            theme: &theme,
            focus: &focus,
            frame_time: &frame_time,
            animation: &animation,
        };
        if container.scrollback() {
            container.paint_scrollback(&mut terminal, &mut footer, &ctx)?;
        } else {
            container.draw(&mut terminal, &mut footer, &ctx)?;
        }

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        match event::read()? {
            Event::Resize(w, h) => {
                terminal.backend_mut().handle_terminal_resize(Size {
                    width: w,
                    height: h,
                })?;
                terminal.clear()?;
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if container.scrollback() {
                    let exit_alt = matches!(key.code, KeyCode::Esc)
                        || (ctrl && matches!(key.code, KeyCode::Char('o' | 'O')));
                    if exit_alt {
                        container.set_scrollback(false);
                        let backend = terminal.backend_mut();
                        backend.queue(LeaveAlternateScreen)?;
                        Backend::flush(backend)?;
                    } else {
                        match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Tab => container.select_newer(),
                            KeyCode::BackTab => container.select_older(),
                            KeyCode::Up => container.scroll_up(1),
                            KeyCode::Down => container.scroll_down(1),
                            _ => {
                                let _ = container.handle_block_event(&mut focus, &Event::Key(key));
                            }
                        }
                    }
                } else {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('o' | 'O') if ctrl => {
                            let backend = terminal.backend_mut();
                            backend.queue(EnterAlternateScreen)?;
                            backend.hide_cursor()?;
                            Backend::flush(backend)?;
                            container.set_scrollback(true);
                        }
                        KeyCode::Char('p') => {
                            pushed += 1;
                            container.push(Box::new(Paragraph::new(Line::raw(format!(
                                "[row #{pushed:>3}] pushed via ScrollbackContainer::push"
                            )))));
                        }
                        KeyCode::Char('P') => {
                            pushed += 1;
                            let lines = (0..3)
                                .map(|i| Line::raw(format!("[multi #{pushed:>3} line {i}]")))
                                .collect::<Vec<_>>();
                            container.push(Box::new(Paragraph::new(lines)));
                        }
                        KeyCode::Char('v') => {
                            pushed += 1;
                            let label = pushed;
                            let id = container.push_active(varsize_block(label, VARSIZE_MAX));
                            varsize = Some((id, label, VARSIZE_MAX));
                        }
                        KeyCode::Char('V') => {
                            if let Some((id, label, h)) = varsize {
                                let new_h = if h <= 1 { VARSIZE_MAX } else { h - 1 };
                                container.update_active(id, varsize_block(label, new_h));
                                varsize = Some((id, label, new_h));
                            }
                        }
                        KeyCode::Char('s') => {
                            pushed += 1;
                            container.push(Box::new(FakeShellBlock::new(pushed, 30)));
                        }
                        KeyCode::Char('a') => {
                            // Tiny scripted active-block lifecycle: start an
                            // empty block, replace its contents with growing
                            // text across two draws, then mark safe. Useful
                            // for eyeballing the active → safe promotion path.
                            pushed += 1;
                            let id = container.push_active(Box::new(Paragraph::new(Line::raw(
                                format!("[active #{pushed:>3}] starting…"),
                            ))));
                            let ctx = DrawContext {
                                theme: &theme,
                                focus: &focus,
                                frame_time: &frame_time,
                                animation: &animation,
                            };
                            container.draw(&mut terminal, &mut footer, &ctx)?;
                            container.update_active(
                                id,
                                Box::new(Paragraph::new(Line::raw(format!(
                                    "[active #{pushed:>3}] grew via update_active"
                                )))),
                            );
                            container.mark_safe(id);
                        }
                        KeyCode::Up | KeyCode::Char('K') => {
                            footer_content = footer_content.saturating_add(1).min(20);
                        }
                        KeyCode::Down | KeyCode::Char('J') => {
                            footer_content = footer_content.saturating_sub(1).max(1);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn varsize_block(label: u32, lines: u16) -> Box<dyn Block> {
    let body: Vec<Line<'static>> = (0..lines)
        .map(|i| Line::raw(format!("[varsize #{label:>3}] line {i} of {lines}")))
        .collect();
    Box::new(Paragraph::new(body))
}

fn footer_block(
    safe: u32,
    active: u32,
    committed: usize,
    content_lines: u16,
) -> Box<Paragraph<'static>> {
    let title = format!(
        " container scratch  safe: {safe}  active: {active}  committed: {committed}  footer: {} ",
        content_lines + 2
    );
    let help: Vec<Line<'static>> = vec![
        Line::raw(
            "p push  P multi  a active  v varsize  V shrink  s shell  ^O alt-view  ↑/K grow  ↓/J shrink  q quit",
        ),
        Line::raw(
            "alt-view: Tab / S-Tab select  j/k/u/d scroll within block  Esc back  ↑/↓ window scroll",
        ),
    ];
    let body: Vec<Line<'static>> = (0..content_lines)
        .map(|i| {
            help.get(i as usize)
                .cloned()
                .unwrap_or_else(|| Line::raw(format!("    (filler row {})", i + 1)))
        })
        .collect();
    Box::new(
        Paragraph::new(body).block(widgets::Block::default().borders(Borders::ALL).title(title)),
    )
}

/// Phase D playground stand-in for `ShellOutputBlock`. Owns a fixed
/// body of `lines` source rows plus an `scroll_y` offset (measured in
/// source lines from the tail) so the alt-view selection + j/k/u/d
/// dispatch can be exercised end-to-end without the binary's real
/// `ShellOutputBlock` (which lives in `frances` and would require a
/// reverse dependency).
///
/// Renders as `[fakeshell #N] line K of M` rows, with a `▶` gutter
/// added by the container when the block is the alt-view selection.
const FAKE_SHELL_TAIL: u16 = 10;
const FAKE_SHELL_TAIL_FOCUSED: u16 = 20;

fn fake_shell_tail(selected: bool) -> u16 {
    if selected {
        FAKE_SHELL_TAIL_FOCUSED
    } else {
        FAKE_SHELL_TAIL
    }
}

struct FakeShellBlock {
    label: u32,
    lines: u16,
    scroll_y: u16,
}

impl FakeShellBlock {
    fn new(label: u32, lines: u16) -> Self {
        Self {
            label,
            lines,
            scroll_y: 0,
        }
    }

    fn max_scroll_for(&self, tail: u16) -> u16 {
        self.lines.saturating_sub(tail)
    }

    fn visible_window(&self, tail: u16) -> (u16, u16) {
        let start_offset = self.scroll_y.min(self.max_scroll_for(tail));
        let end = self.lines - start_offset;
        let start = end.saturating_sub(tail);
        (start, end)
    }
}

impl Input for FakeShellBlock {
    fn handle_event(&mut self, _ctx: &mut EventContext<'_>, event: &Event) -> EventOutcome {
        let Event::Key(key) = event else {
            return EventOutcome::Pass;
        };
        if key.kind != KeyEventKind::Press {
            return EventOutcome::Pass;
        }
        // Events only land here when the block is the alt-view
        // selection, so clamp against the focused (expanded) tail.
        let max = self.max_scroll_for(FAKE_SHELL_TAIL_FOCUSED);
        let half = FAKE_SHELL_TAIL_FOCUSED / 2;
        match key.code {
            KeyCode::Char('j') => {
                self.scroll_y = self.scroll_y.saturating_sub(1);
                EventOutcome::Consumed
            }
            KeyCode::Char('k') => {
                self.scroll_y = self.scroll_y.saturating_add(1).min(max);
                EventOutcome::Consumed
            }
            KeyCode::Char('d') => {
                self.scroll_y = self.scroll_y.saturating_sub(half);
                EventOutcome::Consumed
            }
            KeyCode::Char('u') => {
                self.scroll_y = self.scroll_y.saturating_add(half).min(max);
                EventOutcome::Consumed
            }
            _ => EventOutcome::Pass,
        }
    }
}

impl Block for FakeShellBlock {
    fn kind(&self) -> BlockKind {
        // Reuse the `Raw` tag — the playground block has no wire
        // counterpart, and `kind()` is only consulted by serde
        // dispatch (which this playground doesn't exercise).
        BlockKind::Raw
    }

    fn measure(&self, ctx: &BlockMeasureContext<'_>) -> u16 {
        let tail = fake_shell_tail(ctx.selected);
        1 + self.lines.min(tail)
    }

    fn render(&self, ctx: &mut BlockRenderContext<'_>) -> Sigil {
        let header_style = Style::default().fg(Color::Cyan);
        let header = format!(
            "[fakeshell #{:>3}] {} lines (scroll_y={})",
            self.label, self.lines, self.scroll_y
        );
        let tail = fake_shell_tail(ctx.selected);
        let (window_start, window_end) = if ctx.alt_view {
            self.visible_window(tail)
        } else {
            let start = self.lines.saturating_sub(tail);
            (start, self.lines)
        };

        let area = ctx.area;
        let src_y = ctx.src_y;
        let mut src_idx: u16 = 0;
        // Header.
        if src_idx >= src_y {
            let dst = src_idx - src_y;
            if dst < area.height {
                ctx.buf
                    .set_string(area.x, area.y + dst, &header, header_style);
            }
        }
        src_idx += 1;
        // Body window.
        for line_no in window_start..window_end {
            if src_idx >= src_y {
                let dst = src_idx - src_y;
                if dst >= area.height {
                    return Sigil::blank();
                }
                let text = format!("  line {:>3} of {:>3}", line_no + 1, self.lines);
                ctx.buf
                    .set_string(area.x, area.y + dst, &text, Style::default());
            }
            src_idx += 1;
        }
        Sigil::blank()
    }
}
