use std::collections::HashMap;
use std::io::{Stdout, Write, stdout};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::QueueableCommand;
use crossterm::cursor::{self, Show};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::TerminalOptions;
use ratatui::Viewport;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Size;
use ratatui::style::{Color, Style};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, MissedTickBehavior};
use tracing::warn;

use frances_session::events::{
    BlockKind, PermissionRequest, PermissionResponse, PermissionResponseWire, ScrollbackFrame,
    StreamFrame, SurfaceCmd,
};
use frances_session::llm::Usage;
use frances_session::runtime::SessionRuntime;
use frances_session::session::Session;
use frances_tui::scrollback_container::DrawContext;
use frances_tui::{
    AnimationGate, BlockId as ContainerBlockId, EventContext, Focus, FocusManager, FrameTime,
    Input, ScrollbackBackend, ScrollbackContainer, Theme, WallClockFrameTime, Widget,
};

use crate::tui::{Footer, RawBlock, block_for_kind};

pub struct App<'a> {
    pub session: &'a Session,
    pub runtime: Arc<SessionRuntime>,
    /// Event receiver paired with the runtime's
    /// [`frances_session::runtime::EventsChannel`]. Carries initial
    /// scrollback replay, prompt frames, and any mid-cycle
    /// workflow-switch replays.
    pub events: mpsc::UnboundedReceiver<StreamFrame>,
}

enum KeyAction {
    Quit,
    Interrupt,
    Submit,
    Approve,
    Reject,
    Edit,
    EnterScrollback,
}

enum ScrollbackAction {
    Quit,
    Exit,
    ScrollUp(u16),
    ScrollDown(u16),
    SelectNewer,
    SelectOlder,
    BlockKey(KeyEvent),
}

struct ActiveBlock {
    /// `None` until the first `Some(text)` delta materialises the
    /// block — the wire opener for an empty-content push tracks the
    /// id and kind here without ever measuring or rendering. As soon
    /// as any body text arrives we push to the container and remember
    /// its slot.
    container_id: Option<ContainerBlockId>,
    kind: BlockKind,
    text: String,
}

/// Per-protocol-id tracker for the live blocks currently in flight.
/// The container itself supports arbitrarily many simultaneously-active
/// blocks; this map is just the bridge from wire `BlockId`s to the
/// container's local ids plus the accumulated text needed to re-render
/// on each delta.
struct LiveBlocks {
    by_id: HashMap<frances_session::events::BlockId, ActiveBlock>,
}

impl LiveBlocks {
    fn new() -> Self {
        Self {
            by_id: HashMap::new(),
        }
    }

    /// Process one self-describing delta. The first time we see `id`
    /// we record `kind` (and any body text) but only push to the
    /// container once `text` is `Some(_)` — an opener with `text: None`
    /// is a "tracked but not yet rendered" block. Subsequent deltas
    /// update the kind on every call (so ShellOutput state transitions
    /// land in-place); body text accumulates, materialising the block
    /// the first time `text` is `Some(_)`.
    fn delta(
        &mut self,
        container: &mut ScrollbackContainer,
        id: frances_session::events::BlockId,
        kind: BlockKind,
        text: Option<String>,
    ) {
        let entry = self.by_id.entry(id).or_insert_with(|| ActiveBlock {
            container_id: None,
            kind: kind.clone(),
            text: String::new(),
        });
        entry.kind = kind;
        if let Some(t) = text {
            entry.text.push_str(&t);
            match entry.container_id {
                Some(cid) => container
                    .update_active(cid, block_for_kind(entry.kind.clone(), entry.text.clone())),
                None => {
                    // `push_active` consults the block's `safe_on_push`
                    // method; one-shot blocks (e.g. `ToolUseBlock`,
                    // `RawBlock`) self-promote to safe immediately,
                    // suppressing the in-flight spinner overlay and
                    // letting them drain together with the next safe
                    // prefix of `active_order`.
                    let cid = container
                        .push_active(block_for_kind(entry.kind.clone(), entry.text.clone()));
                    entry.container_id = Some(cid);
                }
            }
        } else if let Some(cid) = entry.container_id {
            // Kind-only delta on an already-materialised block — re-render
            // so the new kind's prefix / style takes effect.
            container.update_active(cid, block_for_kind(entry.kind.clone(), entry.text.clone()));
        }
    }

    /// Mark the block at `id` ready to commit. Returns the container
    /// slot if the block was ever materialised; an unmaterialised
    /// block (only ever saw `text: None`) returns `None` silently.
    /// A `BlockStop` for a completely-unknown id is a runtime-side
    /// protocol bug — we warn and return `None`.
    fn stop_or_recover(
        &mut self,
        id: frances_session::events::BlockId,
        frame_label: &'static str,
    ) -> Option<ContainerBlockId> {
        match self.by_id.remove(&id) {
            Some(active) => active.container_id,
            None => {
                warn!(
                    %id,
                    frame = frame_label,
                    "BlockStop for unknown id; recovering",
                );
                None
            }
        }
    }

    /// Drop all live tracking without touching the container. Used on
    /// a scrollback `Reset` frame: the container is about to be cleared, so
    /// every in-memory active entry will go away with it; we just
    /// need to reset the protocol-id → container-id map.
    fn discard(&mut self) {
        self.by_id.clear();
    }
}

type AppTerminal = Terminal<ScrollbackBackend<CrosstermBackend<Stdout>>>;

impl App<'_> {
    pub async fn run(self) -> Result<()> {
        enable_raw_mode().context("enable raw mode")?;
        let outcome = self.run_loop().await;
        let _ = disable_raw_mode();
        let mut out = stdout();
        let _ = out.queue(Show);
        let _ = out.flush();
        println!();
        outcome
    }

    async fn run_loop(self) -> Result<()> {
        let (w, h) = size().context("query terminal size")?;
        let mut term_size = Size {
            width: w,
            height: h,
        };
        let (_, cursor_row) = cursor::position().context("query cursor position")?;

        let backend = ScrollbackBackend::new(CrosstermBackend::new(stdout()), term_size);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )
        .context("init terminal")?;

        let theme = Theme::default();
        let frame_time = WallClockFrameTime::new();
        let animation = AnimationGate::new();
        let mut focus_manager = FocusManager::new();
        let mut focus = Focus::new();
        let mut footer = Footer::new(&mut focus_manager, "type a message…");
        let mut container = ScrollbackContainer::new(cursor_row);
        container.enable_spinner();
        for (i, line) in self.banner_lines().into_iter().enumerate() {
            let style = if i == 0 {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            };
            container.push(Box::new(RawBlock::single_styled(line, style)));
        }

        let mut state = LiveBlocks::new();
        let mut pending_approval: Option<PermissionRequest> = None;
        let mut frame_rx = self.events;
        let mut events = EventStream::new();
        // Busy-indicator text, set by the workflow via `setStatus`.
        // `Some(text)` → footer shows `{spinner} {text}`; `None` →
        // hidden. The host no longer infers this from token flow.
        let mut status: Option<String> = None;
        // Accumulator for the in-flight replayed block during a
        // scrollback burst (between `ScrollbackFrame::Reset` and `End`).
        // `None` outside a burst.
        let mut replay_open: Option<ReplayBlock> = None;
        let mut latest_usage: Option<Usage> = None;
        let mut spinner_tick = time::interval(Duration::from_millis(120));
        spinner_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            redraw(RedrawArgs {
                terminal: &mut terminal,
                container: &mut container,
                footer: &mut footer,
                theme: &theme,
                focus: &mut focus,
                frame_time: &frame_time,
                animation: &animation,
                status: status.as_deref(),
                latest_usage: latest_usage.as_ref(),
            })?;

            tokio::select! {
                Some(event) = events.next() => {
                    let event = event.context("event read")?;
                    match event {
                        Event::Key(key) if container.scrollback() => {
                            if key.kind != KeyEventKind::Press { continue; }
                            let page = scrollback_page(term_size.height);
                            match classify_scrollback_key(&key, page) {
                                ScrollbackAction::Quit => return Ok(()),
                                ScrollbackAction::Exit => {
                                    // Flip the scrollback flag first so a
                                    // failed alt-screen leave doesn't pin
                                    // us in inspector mode forever; the
                                    // next redraw will reconcile.
                                    container.set_scrollback(false);
                                    if let Err(err) = leave_scrollback(&mut terminal) {
                                        warn!(%err, "leave_scrollback failed; continuing");
                                    }
                                }
                                ScrollbackAction::ScrollUp(n) => container.scroll_up(n),
                                ScrollbackAction::ScrollDown(n) => container.scroll_down(n),
                                ScrollbackAction::SelectNewer => container.select_newer(),
                                ScrollbackAction::SelectOlder => container.select_older(),
                                ScrollbackAction::BlockKey(key) => {
                                    let _ = container
                                        .handle_block_event(&mut focus, &Event::Key(key));
                                }
                            }
                        }
                        Event::Key(key) => {
                            if key.kind != KeyEventKind::Press { continue; }
                            match classify_key(&key, pending_approval.is_some()) {
                                KeyAction::Quit => return Ok(()),
                                KeyAction::Interrupt => self.runtime.interrupt(),
                                KeyAction::EnterScrollback => {
                                    // If the alt-screen escape sequence
                                    // fails, leave the flag clear so we
                                    // stay in live mode rather than
                                    // entering a half-applied inspector.
                                    match enter_scrollback(&mut terminal) {
                                        Ok(()) => container.set_scrollback(true),
                                        Err(err) => {
                                            warn!(%err, "enter_scrollback failed; staying in live mode");
                                        }
                                    }
                                }
                                KeyAction::Submit => {
                                    if footer.input.is_empty() { continue; }
                                    let text = footer.input.text();
                                    footer.input.clear();
                                    if let Some(req) = pending_approval.take() {
                                        footer.input.set_placeholder("type a message…");
                                        respond_permission(
                                            &self.runtime,
                                            req.reply,
                                            PermissionResponseWire::RedirectToChat { content: text },
                                        );
                                    } else {
                                        self.runtime.prompt(text);
                                    }
                                }
                                KeyAction::Approve | KeyAction::Reject => {
                                    let Some(req) = pending_approval.take() else { continue; };
                                    let details = if footer.input.is_empty() {
                                        None
                                    } else {
                                        Some(footer.input.text())
                                    };
                                    footer.input.clear();
                                    footer.input.set_placeholder("type a message…");
                                    let response = match classify_key(&key, true) {
                                        KeyAction::Approve => {
                                            PermissionResponseWire::Yes { details }
                                        }
                                        _ => PermissionResponseWire::No { details },
                                    };
                                    respond_permission(&self.runtime, req.reply, response);
                                }
                                KeyAction::Edit => {
                                    let mut redraw = false;
                                    let mut ctx = EventContext {
                                        focus: &mut focus,
                                        redraw: &mut redraw,
                                    };
                                    let _ = footer.handle_event(&mut ctx, &Event::Key(key));
                                }
                            }
                        }
                        Event::Resize(width, height) => {
                            term_size = Size { width, height };
                            if let Err(err) =
                                terminal.backend_mut().handle_terminal_resize(term_size)
                            {
                                warn!(%err, "handle_terminal_resize failed; redraw will reconcile");
                            }
                            if let Err(err) = terminal.clear() {
                                warn!(%err, "terminal clear after resize failed; continuing");
                            }
                        }
                        _ => {}
                    }
                }
                _ = spinner_tick.tick(), if animation.active() > 0 => {
                    // Wake-up only — animated widgets hold leases on
                    // `animation`; this branch is disabled when no one
                    // does. Repaint happens at the top of the loop.
                }
                Some(frame) = frame_rx.recv() => {
                    // The busy indicator is workflow-driven: a `Surface`
                    // frame sets or clears the footer. (Scrollback frames
                    // carry no chrome — they're their own variant now.)
                    if let StreamFrame::Surface(cmd) = &frame {
                        status = match cmd {
                            SurfaceCmd::SetFooter { text } => Some(text.clone()),
                            SurfaceCmd::ClearFooter => None,
                        };
                    }
                    if let Some(req) = handle_frame(
                        &mut terminal,
                        &mut container,
                        &mut state,
                        &mut replay_open,
                        frame,
                        &mut latest_usage,
                    )? {
                        footer.input.set_placeholder(PERMISSION_PLACEHOLDER);
                        pending_approval = Some(req);
                    }
                }
            }
        }
    }

    fn banner_lines(&self) -> Vec<String> {
        vec![
            format!("frances session {}", self.session.id),
            "  Enter to send. Alt+Enter for newline. Esc to interrupt. Ctrl-O for history. Ctrl-C or Ctrl-D to exit.".to_string(),
        ]
    }
}

/// Braille-dot frames cycled through the busy indicator. Same glyph
/// set the container uses for its active-block spinner; sharing the
/// vocabulary keeps the two animations feeling like one family.
const STATUS_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Render the workflow-set status text as `{spinner} [text]`, sharing
/// the bracketed-pill convention with block headers (shell, reasoning).
/// The [`TextInput`](frances_tui::TextInput) widget paints it as dim
/// text with a single bright cell pulsing across the line; the colour
/// here picks the hue of that pulse. `frame` is in 60fps units; the
/// glyph advances one cell every six frames (~10 Hz).
fn status_line(text: &str, frame: f64) -> (String, Color) {
    let glyph = STATUS_FRAMES[((frame / 6.0) as usize) % STATUS_FRAMES.len()];
    (format!("{glyph} [{text}]"), Color::Cyan)
}

struct RedrawArgs<'a> {
    terminal: &'a mut AppTerminal,
    container: &'a mut ScrollbackContainer,
    footer: &'a mut Footer,
    theme: &'a Theme,
    focus: &'a mut Focus,
    frame_time: &'a dyn FrameTime,
    animation: &'a AnimationGate,
    status: Option<&'a str>,
    latest_usage: Option<&'a Usage>,
}

fn redraw(args: RedrawArgs<'_>) -> std::io::Result<()> {
    let RedrawArgs {
        terminal,
        container,
        footer,
        theme,
        focus,
        frame_time,
        animation,
        status,
        latest_usage,
    } = args;

    footer
        .input
        .set_status(status.map(|text| status_line(text, frame_time.get_frame())));
    let token_text = latest_usage
        .map(format_token_status)
        .unwrap_or_else(|| "tokens: —".to_string());
    footer.status.set_text(token_text);

    // Rebuild focus's tree-order list from this frame's footer
    // before any event dispatch needs it.
    focus.refresh(footer as &dyn Widget);

    let ctx = DrawContext {
        theme,
        focus,
        frame_time,
        animation,
    };

    if container.scrollback() {
        container.paint_scrollback(terminal, footer, &ctx)?;
        let backend = terminal.backend_mut();
        backend.hide_cursor()?;
        Backend::flush(backend)?;
        return Ok(());
    }

    container.draw(terminal, footer, &ctx)?;

    // The TextInput's underlying TextArea paints its own cursor cell
    // (reversed style) and handles horizontal scroll when the line
    // outgrows the inner width, so the terminal cursor stays hidden.
    let backend = terminal.backend_mut();
    backend.hide_cursor()?;
    Backend::flush(backend)?;
    Ok(())
}

/// Per-replay scratchpad: the same {id, kind, accumulated text}
/// triple the runtime uses, mirrored on the TUI side. Carried in
/// `App` state for the lifetime of one replay burst (between
/// `ScrollbackFrame::Reset` and `ScrollbackFrame::End`).
struct ReplayBlock {
    id: frances_session::events::BlockId,
    kind: BlockKind,
    text: String,
}

fn handle_frame(
    terminal: &mut AppTerminal,
    container: &mut ScrollbackContainer,
    state: &mut LiveBlocks,
    replay_open: &mut Option<ReplayBlock>,
    frame: StreamFrame,
    latest_usage: &mut Option<Usage>,
) -> Result<Option<PermissionRequest>> {
    match frame {
        StreamFrame::Scrollback(sf) => {
            handle_replay_frame(terminal, container, state, replay_open, sf)?;
        }
        StreamFrame::BlockDelta { id, kind, text } => {
            state.delta(container, id, kind, text);
        }
        StreamFrame::BlockStop { id } => {
            if let Some(container_id) = state.stop_or_recover(id, "BlockStop") {
                container.mark_safe(container_id);
            }
        }
        StreamFrame::BlockTruncated { id } => {
            // The live wire shouldn't emit truncated stops — recover the
            // same way as a stop for an unknown id (warn + mark safe).
            if let Some(container_id) = state.stop_or_recover(id, "BlockTruncated") {
                container.mark_safe(container_id);
            }
        }
        StreamFrame::Usage(usage) => {
            *latest_usage = Some(usage);
        }
        StreamFrame::Surface(_) => {
            // The busy indicator is applied in the run loop before
            // `handle_frame`; nothing to do here.
        }
        StreamFrame::Error(message) => {
            // Error frames are a side-channel — they don't seal any
            // open block. They render below whatever's in flight.
            container.push(Box::new(RawBlock::single_styled(
                format!("frances: error: {message}"),
                Style::default().fg(Color::Red),
            )));
        }
        StreamFrame::Permission(request) => {
            container.push(Box::new(RawBlock::single_styled(
                format!("permission: {}", request.prompt),
                Style::default().fg(Color::Yellow),
            )));
            return Ok(Some(request));
        }
    }
    Ok(None)
}

/// Apply one frame of a scrollback-replay burst. Replayed blocks are
/// accumulated in `open` (one in flight at a time) and handed to the
/// container's `push_committed[_truncated]` on stop — they never touch
/// the live `active` deque. Matches the bounded [`ScrollbackFrame`] set
/// exhaustively; no live-only frames reach here.
fn handle_replay_frame(
    terminal: &mut AppTerminal,
    container: &mut ScrollbackContainer,
    state: &mut LiveBlocks,
    open: &mut Option<ReplayBlock>,
    frame: ScrollbackFrame,
) -> Result<()> {
    match frame {
        ScrollbackFrame::Reset { .. } => {
            // Drop live block tracking, clear the container, and start a
            // fresh burst. `clear()` force-spills current screen content
            // into native scrollback so the user keeps their terminal
            // history; the in-memory deques reset so the inspector shows
            // only the replayed workflow.
            state.discard();
            container.clear(terminal)?;
            *open = None;
        }
        ScrollbackFrame::Block { id, kind, text } => match open.as_mut() {
            Some(b) if b.id == id => {
                b.kind = kind;
                if let Some(t) = text {
                    b.text.push_str(&t);
                }
            }
            // Nothing open, or a new id: drop any orphan (the producer
            // closes every block it opens) and start fresh.
            _ => {
                *open = Some(ReplayBlock {
                    id,
                    kind,
                    text: text.unwrap_or_default(),
                });
            }
        },
        ScrollbackFrame::BlockStop { id } => {
            if let Some(b) = open.take_if(|b| b.id == id) {
                container.push_committed(block_for_kind(b.kind, b.text));
            }
        }
        ScrollbackFrame::BlockTruncated { id } => {
            if let Some(b) = open.take_if(|b| b.id == id) {
                container.push_committed_truncated(block_for_kind(b.kind, b.text));
            }
        }
        ScrollbackFrame::Error(message) => {
            container.push_committed(Box::new(RawBlock::single_styled(
                format!("frances: error: {message}"),
                Style::default().fg(Color::Red),
            )));
        }
        ScrollbackFrame::End => {
            // Any half-built block at end is dropped; the producer
            // closes every block it opens.
            *open = None;
        }
    }
    Ok(())
}

const PERMISSION_PLACEHOLDER: &str =
    "Alt+Y yes  Alt+N no  Enter chat (text becomes details for yes/no)";

fn respond_permission(
    runtime: &Arc<SessionRuntime>,
    reply: oneshot::Sender<PermissionResponse>,
    response: PermissionResponseWire,
) {
    if let Err(error) = runtime.respond_permission(reply, response) {
        runtime
            .events
            .send(StreamFrame::Error(format!("permission: {error}")));
    }
}

fn format_token_status(usage: &Usage) -> String {
    format!(
        "tokens: {} total · {} prompt ({} cached) · {} completion",
        usage.total_tokens, usage.prompt_tokens, usage.cached_input_tokens, usage.completion_tokens
    )
}

fn classify_key(key: &KeyEvent, pending_approval: bool) -> KeyAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        // Esc interrupts the running workflow (delivered to its inbox);
        // it no longer quits. The app exits via Ctrl-C / Ctrl-D.
        KeyCode::Esc => KeyAction::Interrupt,
        KeyCode::Char('c' | 'd') if ctrl => KeyAction::Quit,
        KeyCode::Char('o' | 'O') if ctrl && !pending_approval => KeyAction::EnterScrollback,
        KeyCode::Char('y' | 'Y') if alt && pending_approval => KeyAction::Approve,
        KeyCode::Char('n' | 'N') if alt && pending_approval => KeyAction::Reject,
        KeyCode::Enter if !alt => KeyAction::Submit,
        _ => {
            let _ = (ctrl, alt);
            KeyAction::Edit
        }
    }
}

fn classify_scrollback_key(key: &KeyEvent, page: u16) -> ScrollbackAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => ScrollbackAction::Exit,
        KeyCode::Char('c' | 'd') if ctrl => ScrollbackAction::Quit,
        KeyCode::Char('o' | 'O') if ctrl => ScrollbackAction::Exit,
        KeyCode::Up => ScrollbackAction::ScrollUp(1),
        KeyCode::Down => ScrollbackAction::ScrollDown(1),
        KeyCode::PageUp => ScrollbackAction::ScrollUp(page),
        KeyCode::PageDown => ScrollbackAction::ScrollDown(page),
        KeyCode::Home => ScrollbackAction::ScrollUp(u16::MAX),
        KeyCode::End => ScrollbackAction::ScrollDown(u16::MAX),
        // Tab / Shift-Tab move block selection inside the inspector.
        // Crossterm reports Shift-Tab as `BackTab` regardless of the
        // modifier bits, so a bare `KeyCode::BackTab` is enough.
        KeyCode::Tab => ScrollbackAction::SelectNewer,
        KeyCode::BackTab => ScrollbackAction::SelectOlder,
        // Everything else is forwarded to the selected block's
        // `Input::handle_event`. Blocks that don't care return
        // `Pass`, which we discard.
        _ => ScrollbackAction::BlockKey(*key),
    }
}

/// Rows to scroll on PageUp / PageDown. Leaves a 1-row anchor of
/// visible content above/below the new window so the user can see
/// where they came from.
fn scrollback_page(terminal_h: u16) -> u16 {
    // Content area = terminal_h - 2 status bars - footer (textarea + token row).
    terminal_h.saturating_sub(7).max(1)
}

fn enter_scrollback(terminal: &mut AppTerminal) -> Result<()> {
    let backend = terminal.backend_mut();
    backend
        .queue(EnterAlternateScreen)
        .context("enter alt screen")?;
    backend.hide_cursor()?;
    Backend::flush(backend)?;
    Ok(())
}

fn leave_scrollback(terminal: &mut AppTerminal) -> Result<()> {
    let backend = terminal.backend_mut();
    backend
        .queue(LeaveAlternateScreen)
        .context("leave alt screen")?;
    Backend::flush(backend)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives `LiveBlocks::delta` against a real `ScrollbackContainer`.
    /// Verifies that two distinct ids coexist as concurrently-open
    /// active blocks (no implicit close), same-id deltas append, and
    /// each block closes independently via `stop_or_recover`.
    #[test]
    fn live_blocks_two_ids_coexist_and_close_independently() {
        let mut container = ScrollbackContainer::new(0);
        let mut state = LiveBlocks::new();
        let kind_a = BlockKind::Text {
            source: frances_session::events::Source::User,
        };
        let kind_b = BlockKind::ToolUse {
            name: "shell".into(),
            detail: None,
        };

        // Open id 1.
        state.delta(
            &mut container,
            frances_session::events::BlockId(1),
            kind_a.clone(),
            Some("hel".to_owned()),
        );
        assert_eq!(container.active_count(), 1);

        // Same-id append.
        state.delta(
            &mut container,
            frances_session::events::BlockId(1),
            kind_a,
            Some("lo".to_owned()),
        );
        assert_eq!(container.active_count(), 1);

        // Different-id delta: opens a second active alongside the
        // first. Neither closes.
        state.delta(
            &mut container,
            frances_session::events::BlockId(2),
            kind_b,
            Some("ls".to_owned()),
        );
        assert_eq!(container.active_count(), 2);
        assert_eq!(container.safe_count(), 0);

        // Close id 2 first — its slot drains; id 1 is still active
        // but stuck behind id 2 in container order, so it's now also
        // ready to commit (mark_safe drains the contiguous prefix).
        let b_id = state
            .stop_or_recover(frances_session::events::BlockId(2), "BlockStop")
            .expect("stop id 2");
        container.mark_safe(b_id);

        // Close id 1.
        let a_id = state
            .stop_or_recover(frances_session::events::BlockId(1), "BlockStop")
            .expect("stop id 1");
        container.mark_safe(a_id);

        assert_eq!(container.active_count(), 0);
        assert_eq!(container.safe_count(), 2);
    }

    /// `BlockStop` for an id we don't have open is a protocol bug on
    /// the runtime side. The TUI should not bail back to the CLI — it
    /// should log and continue. `stop_or_recover` returns `None`.
    #[test]
    fn block_stop_for_unknown_id_does_not_bail() {
        let container = ScrollbackContainer::new(0);
        let mut state = LiveBlocks::new();

        let result = state.stop_or_recover(frances_session::events::BlockId(99), "BlockStop");

        assert!(result.is_none(), "unknown id stop returns None");
        assert_eq!(container.active_count(), 0);
        assert_eq!(container.safe_count(), 0);
        assert_eq!(container.committed_count(), 0);
    }

    /// A run of `text: None` deltas between two completed blocks —
    /// covering each `Source` variant, plus closing some of them while
    /// leaving others tracked — must not insert anything into the
    /// container. Guards against the case where a
    /// `transcript.push(new MarkdownFrame({ source }))` /
    /// `transcript.push(new MarkdownFrame({ content: null }))` pair
    /// would somehow surface as a blank row between the real frames
    /// either side of it.
    #[test]
    fn none_text_frames_between_completed_blocks_add_no_rows() {
        use frances_session::events::Source;
        let mut container = ScrollbackContainer::new(0);
        let mut state = LiveBlocks::new();
        let assistant = || BlockKind::Text {
            source: Source::Assistant,
        };
        let internal = || BlockKind::Text {
            source: Source::Internal,
        };
        let user = || BlockKind::Text {
            source: Source::User,
        };

        // Completed block A.
        state.delta(
            &mut container,
            frances_session::events::BlockId(1),
            assistant(),
            Some("first".to_owned()),
        );
        let a_id = state
            .stop_or_recover(frances_session::events::BlockId(1), "BlockStop")
            .expect("close A");
        container.mark_safe(a_id);
        assert_eq!(container.safe_count(), 1);
        assert_eq!(container.active_count(), 0);

        // None-content frames in between: mixed senders, some closed,
        // some still tracked when block B opens.
        state.delta(
            &mut container,
            frances_session::events::BlockId(2),
            assistant(),
            None,
        );
        state.delta(
            &mut container,
            frances_session::events::BlockId(3),
            internal(),
            None,
        );
        state.delta(
            &mut container,
            frances_session::events::BlockId(4),
            user(),
            None,
        );
        state.delta(
            &mut container,
            frances_session::events::BlockId(5),
            internal(),
            None,
        );
        assert_eq!(
            container.active_count(),
            0,
            "no None-text frame may materialise"
        );

        // Closing a tracked-but-unmaterialised frame must return None
        // (no container id to mark safe) and not touch the container.
        assert!(
            state
                .stop_or_recover(frances_session::events::BlockId(2), "BlockStop")
                .is_none()
        );
        assert!(
            state
                .stop_or_recover(frances_session::events::BlockId(4), "BlockStop")
                .is_none()
        );
        assert_eq!(container.active_count(), 0);
        assert_eq!(container.safe_count(), 1);

        // Completed block B opens with ids 3 and 5 still tracked but
        // unrendered — they must not affect ordering or counts.
        state.delta(
            &mut container,
            frances_session::events::BlockId(6),
            assistant(),
            Some("second".to_owned()),
        );
        let b_id = state
            .stop_or_recover(frances_session::events::BlockId(6), "BlockStop")
            .expect("close B");
        container.mark_safe(b_id);

        assert_eq!(container.active_count(), 0);
        assert_eq!(
            container.safe_count(),
            2,
            "exactly two materialised blocks; None-content frames contributed nothing"
        );
        assert_eq!(container.committed_count(), 0);
    }

    /// `BlockStop` with an id that doesn't match any open block must
    /// leave the existing opens alone — multi-open means an unknown
    /// stop is a side event, not a "close the current block" signal.
    #[test]
    fn block_stop_with_unknown_id_leaves_others_open() {
        let mut container = ScrollbackContainer::new(0);
        let mut state = LiveBlocks::new();
        let kind = BlockKind::Text {
            source: frances_session::events::Source::User,
        };

        // Open id 1.
        state.delta(
            &mut container,
            frances_session::events::BlockId(1),
            kind,
            Some("hi".to_owned()),
        );
        assert_eq!(container.active_count(), 1);

        // Stop with id 2 — unknown. Returns None; id 1 stays open.
        let recovered = state.stop_or_recover(frances_session::events::BlockId(2), "BlockStop");
        assert!(recovered.is_none());
        assert_eq!(container.active_count(), 1);
        assert_eq!(container.safe_count(), 0);
        assert_eq!(state.by_id.len(), 1);
    }
}
