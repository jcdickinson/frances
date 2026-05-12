//! Scratchpad for `ScrollbackContainer`.
//!
//! Mirrors `frances-tui-scratch` but exercises the new container path
//! instead of calling `BottomBackend::emit_above` directly. The
//! footer block reports its own height; the container measures it,
//! decides which history rows fit above, and paints. Rows that fall
//! off the top are flag-marked but (today) not yet emitted into
//! native scrollback — that primitive lands in a follow-up.
//!
//! Controls:
//!   p          push a single-line history row (lands in `safe`, or
//!              queues at the back of `active` flagged ready-to-promote
//!              if older active blocks are still in flight)
//!   P          push a 3-line history row (same path as `p`)
//!   a          start an active block, append text, mark safe
//!   v          push a varsize active block (starts at 5 rows)
//!   V          shrink the varsize block by 1, cycling back to 5 at 1
//!   ↑ / k      grow footer by 1 row
//!   ↓ / j      shrink footer by 1 row
//!   q / Esc    quit

use std::io::{self, Write, stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use crossterm::{QueueableCommand, cursor};
use ratatui::Terminal;
use ratatui::TerminalOptions;
use ratatui::Viewport;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Size;
use ratatui::text::Line;
use ratatui::widgets;
use ratatui::widgets::{Borders, Paragraph};

use frances_tui::{Block, BlockId, InlineBackend, ScrollbackContainer};

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
    let backend = InlineBackend::new(CrosstermBackend::new(stdout()), term_size);
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
    let mut container = ScrollbackContainer::new(footer_block(0, 0, 0, footer_content), cursor_row);
    let mut pushed: u32 = 0;
    // Most recently pushed varsize block — `V` cycles its height
    // through the `update_active` path so the footer-pin behaviour
    // around content shrinkage can be eyeballed against new pushes.
    let mut varsize: Option<(BlockId, u32, u16)> = None;

    loop {
        container.set_footer(footer_block(
            container.safe_count() as u32,
            container.active_count() as u32,
            container.committed_count(),
            footer_content,
        ));
        container.draw(&mut terminal)?;

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
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
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
                KeyCode::Char('a') => {
                    // Tiny scripted active-block lifecycle: start an
                    // empty block, replace its contents with growing
                    // text across two draws, then mark safe. Useful
                    // for eyeballing the active → safe promotion path.
                    pushed += 1;
                    let id = container.push_active(Box::new(Paragraph::new(Line::raw(format!(
                        "[active #{pushed:>3}] starting…"
                    )))));
                    container.draw(&mut terminal)?;
                    container.update_active(
                        id,
                        Box::new(Paragraph::new(Line::raw(format!(
                            "[active #{pushed:>3}] grew via update_active"
                        )))),
                    );
                    container.mark_safe(id);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    footer_content = footer_content.saturating_add(1).min(20);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    footer_content = footer_content.saturating_sub(1).max(1);
                }
                _ => {}
            },
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

fn footer_block(safe: u32, active: u32, committed: usize, content_lines: u16) -> Box<dyn Block> {
    let title = format!(
        " container scratch  safe: {safe}  active: {active}  committed: {committed}  footer: {} ",
        content_lines + 2
    );
    let help: Vec<Line<'static>> = vec![
        Line::raw(
            "p push   P multi   a active   v varsize   V shrink-varsize   ↑/k grow-footer   ↓/j shrink-footer   q quit",
        ),
        Line::raw("oldest safe blocks spill into native scrollback when they don't fit."),
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
