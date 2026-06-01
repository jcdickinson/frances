use std::collections::HashMap;
use std::io::{Stdout, Write, stdout};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::QueueableCommand;
use crossterm::cursor::{self, Show};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
    supports_keyboard_enhancement,
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
    PermissionRequest, PermissionResponse, PermissionResponseWire, ScrollbackFrame, SectionApply,
    SectionId, SectionKind, StreamFrame, SurfaceCmd,
};
use frances_session::llm::Usage;
use frances_session::runtime::SessionRuntime;
use frances_session::session::Session;
use frances_tui::block::Sigil;
use frances_tui::scrollback_container::DrawContext;
use frances_tui::{
    AnimationGate, Block, BlockId as ContainerBlockId, EventContext, Focus, FocusManager,
    FrameTime, Input, ScrollbackBackend, ScrollbackContainer, Section, SectionView, Theme,
    WallClockFrameTime, Widget,
};

use crate::tui::sections::make_section;
use crate::tui::{Footer, RawBlock};

pub struct App<'a> {
    pub session: &'a Session,
    pub runtime: Arc<SessionRuntime>,
    /// Event receiver paired with the runtime's
    /// [`frances_session::runtime::EventsChannel`]. Carries initial
    /// scrollback replay, prompt frames, and any mid-cycle
    /// workflow-switch replays.
    pub events: mpsc::UnboundedReceiver<StreamFrame>,
}

#[derive(Debug, PartialEq, Eq)]
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

struct LiveSection {
    section: Box<dyn Section>,
    /// Container id for this section's single [`SectionView`] entry.
    /// `None` until the section's first apply produces renderable
    /// content (empty sections push nothing).
    block_id: Option<ContainerBlockId>,
}

/// Per-section-id tracker for the live sections currently in flight.
/// Each section renders as ONE container entry — a [`SectionView`] that
/// owns the section's inner blocks and paints its own streaming
/// indicator while open. On every event the dispatcher re-applies, wraps
/// the section's fresh block list in a new `SectionView`, and replaces
/// the entry.
struct LiveBlocks {
    by_id: HashMap<SectionId, LiveSection>,
}

impl LiveBlocks {
    fn new() -> Self {
        Self {
            by_id: HashMap::new(),
        }
    }

    /// Process one self-describing `SectionAppend`. The first time we
    /// see `id` we construct the section via [`make_section`]; subsequent
    /// appends apply the event and refresh the container entry. The
    /// section is still open, so the view streams.
    fn append(
        &mut self,
        container: &mut ScrollbackContainer,
        id: SectionId,
        kind: SectionKind,
        delta: String,
    ) {
        let entry = self.by_id.entry(id).or_insert_with(|| LiveSection {
            section: make_section(&kind),
            block_id: None,
        });
        let blocks = entry.section.apply(SectionApply::Append {
            kind: &kind,
            delta: &delta,
        });
        let sigil = entry.section.sigil();
        sync_view(container, &mut entry.block_id, blocks, sigil, true);
    }

    /// Seal a section: run a `Close` apply, refresh the entry with a
    /// non-streaming view, then mark it safe in the container. Returns
    /// `true` when the section was known. A close for an unknown id is a
    /// protocol bug — we warn and continue.
    fn close(
        &mut self,
        container: &mut ScrollbackContainer,
        id: SectionId,
        frame_label: &'static str,
    ) -> bool {
        self.seal(container, id, SectionApply::Close, frame_label)
    }

    /// Truncate variant of [`Self::close`] — same flow with a
    /// `Truncate` apply for sections that distinguish truncation from
    /// clean close.
    fn truncate(
        &mut self,
        container: &mut ScrollbackContainer,
        id: SectionId,
        frame_label: &'static str,
    ) -> bool {
        self.seal(container, id, SectionApply::Truncate, frame_label)
    }

    /// Shared close/truncate path: apply the sealing event, rebuild the
    /// view with `streaming = false`, and promote it out of `active`.
    fn seal(
        &mut self,
        container: &mut ScrollbackContainer,
        id: SectionId,
        event: SectionApply<'_>,
        frame_label: &'static str,
    ) -> bool {
        let Some(mut entry) = self.by_id.remove(&id) else {
            warn!(
                section_id = id.0,
                frame = frame_label,
                "seal for unknown section id; recovering",
            );
            return false;
        };
        let blocks = entry.section.apply(event);
        let sigil = entry.section.sigil();
        sync_view(container, &mut entry.block_id, blocks, sigil, false);
        if let Some(bid) = entry.block_id {
            container.mark_safe(bid);
        }
        true
    }

    /// Drop all live tracking without touching the container. Used on
    /// a scrollback `Reset` frame: the container is about to be
    /// cleared, so every in-memory active entry will go away with it;
    /// we just need to reset the section map.
    fn discard(&mut self) {
        self.by_id.clear();
    }
}

/// Refresh a section's single container entry. Wraps the section's
/// current block list in a fresh [`SectionView`] and either updates the
/// existing entry or pushes a new one. An empty block list pushes
/// nothing — an unmaterialised section has no entry yet.
fn sync_view(
    container: &mut ScrollbackContainer,
    block_id: &mut Option<ContainerBlockId>,
    blocks: Vec<Box<dyn Block>>,
    sigil: Sigil,
    streaming: bool,
) {
    if blocks.is_empty() {
        return;
    }
    let view = Box::new(SectionView::new(blocks, sigil, streaming));
    match *block_id {
        Some(bid) => container.update_active(bid, view),
        None => *block_id = Some(container.push_active(view)),
    }
}

type AppTerminal = Terminal<ScrollbackBackend<CrosstermBackend<Stdout>>>;

impl App<'_> {
    pub async fn run(self) -> Result<()> {
        enable_raw_mode().context("enable raw mode")?;
        // Enable the kitty keyboard protocol where the terminal supports
        // it, so modifiers are reported on keys that legacy encoding
        // leaves ambiguous — notably Shift+Enter, which otherwise sends
        // the same bytes as a bare Enter. Without this `classify_key`
        // never sees the Shift bit and Shift+Enter submits instead of
        // inserting a newline.
        let enhanced_keys = matches!(supports_keyboard_enhancement(), Ok(true));
        if enhanced_keys {
            let mut out = stdout();
            let _ = out.queue(PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
            ));
            let _ = out.flush();
        }

        let outcome = self.run_loop().await;

        let mut out = stdout();
        if enhanced_keys {
            let _ = out.queue(PopKeyboardEnhancementFlags);
        }
        let _ = out.queue(Show);
        let _ = out.flush();
        let _ = disable_raw_mode();
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
        container.enable_animation();
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
        // `Some(text)` → footer shows `{spinner} {text}`; `None` → hidden.
        let mut status: Option<String> = None;
        // Accumulator for the in-flight replayed block during a
        // scrollback burst (between `ScrollbackFrame::Reset` and `End`).
        // `None` outside a burst.
        let mut replay_open: Option<ReplaySection> = None;
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
                    // frame sets or clears the footer.
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
            "  Enter to send. Shift+Enter or Alt+Enter for newline. Esc to interrupt. Ctrl-O for history. Ctrl-C or Ctrl-D to exit.".to_string(),
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

/// Per-replay scratchpad: the section currently being replayed.
/// Carried in `App` state for the lifetime of one replay burst
/// (between [`ScrollbackFrame::Reset`] and [`ScrollbackFrame::End`]).
/// At most one section is open at a time during replay — each
/// persisted row is replayed as Append + Close (or Truncated) before
/// the next one starts.
struct ReplaySection {
    id: SectionId,
    section: Box<dyn Section>,
}

fn handle_frame(
    terminal: &mut AppTerminal,
    container: &mut ScrollbackContainer,
    state: &mut LiveBlocks,
    replay_open: &mut Option<ReplaySection>,
    frame: StreamFrame,
    latest_usage: &mut Option<Usage>,
) -> Result<Option<PermissionRequest>> {
    match frame {
        StreamFrame::Scrollback(sf) => {
            handle_replay_frame(terminal, container, state, replay_open, sf)?;
        }
        StreamFrame::SectionAppend { id, kind, delta } => {
            state.append(container, id, kind, delta);
        }
        StreamFrame::SectionClose { id } => {
            state.close(container, id, "SectionClose");
        }
        StreamFrame::SectionTruncated { id } => {
            // The live wire shouldn't emit truncated closes — recover
            // the same way as a close for an unknown id (warn + mark
            // safe).
            state.truncate(container, id, "SectionTruncated");
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
    open: &mut Option<ReplaySection>,
    frame: ScrollbackFrame,
) -> Result<()> {
    match frame {
        ScrollbackFrame::Reset { .. } => {
            state.discard();
            container.clear(terminal)?;
            *open = None;
        }
        ScrollbackFrame::SectionAppend { id, kind, delta } => {
            // Drop any orphan (the producer closes every section it
            // opens) and start fresh.
            let mut section = make_section(&kind);
            // Discard the blocks returned here — replay applies Append
            // and then immediately Close, so we commit the final block
            // list from the Close path below.
            let _ = section.apply(SectionApply::Append {
                kind: &kind,
                delta: &delta,
            });
            *open = Some(ReplaySection { id, section });
        }
        ScrollbackFrame::SectionClose { id } => {
            if let Some(mut s) = open.take_if(|s| s.id == id) {
                let blocks = s.section.apply(SectionApply::Close);
                if !blocks.is_empty() {
                    let view = Box::new(SectionView::new(blocks, s.section.sigil(), false));
                    container.push_committed(view);
                }
            }
        }
        ScrollbackFrame::SectionTruncated { id } => {
            if let Some(mut s) = open.take_if(|s| s.id == id) {
                let blocks = s.section.apply(SectionApply::Truncate);
                if !blocks.is_empty() {
                    let view = Box::new(SectionView::new(blocks, s.section.sigil(), false));
                    container.push_committed_truncated(view);
                }
            }
        }
        ScrollbackFrame::Error(message) => {
            container.push_committed(Box::new(RawBlock::single_styled(
                format!("frances: error: {message}"),
                Style::default().fg(Color::Red),
            )));
        }
        ScrollbackFrame::End => {
            // Any half-built section at end is dropped; the producer
            // closes every section it opens.
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
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    match key.code {
        // Esc interrupts the running workflow (delivered to its inbox).
        KeyCode::Esc => KeyAction::Interrupt,
        KeyCode::Char('c' | 'd') if ctrl => KeyAction::Quit,
        KeyCode::Char('o' | 'O') if ctrl && !pending_approval => KeyAction::EnterScrollback,
        KeyCode::Char('y' | 'Y') if alt && pending_approval => KeyAction::Approve,
        KeyCode::Char('n' | 'N') if alt && pending_approval => KeyAction::Reject,
        KeyCode::Enter if !alt && !shift => KeyAction::Submit,
        _ => {
            let _ = (ctrl, alt, shift);
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
    use crossterm::event::KeyEventKind;
    use frances_session::events::Source;

    fn enter(modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Enter, modifiers, KeyEventKind::Press)
    }

    /// Bare Enter submits; Enter with Shift or Alt falls through to
    /// `Edit` so the textarea inserts a newline instead of submitting.
    #[test]
    fn enter_submits_only_without_shift_or_alt() {
        assert_eq!(
            classify_key(&enter(KeyModifiers::NONE), false),
            KeyAction::Submit
        );
        assert_eq!(
            classify_key(&enter(KeyModifiers::SHIFT), false),
            KeyAction::Edit
        );
        assert_eq!(
            classify_key(&enter(KeyModifiers::ALT), false),
            KeyAction::Edit
        );
    }

    /// Drives [`LiveBlocks::append`] / [`LiveBlocks::close`] against a
    /// real `ScrollbackContainer`. Two distinct section ids coexist as
    /// concurrently-open active blocks, same-id appends extend, and
    /// each section closes independently.
    #[test]
    fn live_sections_two_ids_coexist_and_close_independently() {
        let mut container = ScrollbackContainer::new(0);
        let mut state = LiveBlocks::new();
        let kind_a = SectionKind::Markdown {
            source: Source::User,
        };
        let kind_b = SectionKind::ToolUse {
            name: "shell".into(),
            detail: None,
        };

        state.append(&mut container, SectionId(1), kind_a.clone(), "hel".into());
        assert_eq!(container.active_count(), 1);

        state.append(&mut container, SectionId(1), kind_a, "lo".into());
        assert_eq!(container.active_count(), 1);

        state.append(&mut container, SectionId(2), kind_b, "".into());
        assert_eq!(container.active_count(), 2);
        assert_eq!(container.safe_count(), 0);

        assert!(state.close(&mut container, SectionId(2), "test"));
        assert!(state.close(&mut container, SectionId(1), "test"));

        assert_eq!(container.active_count(), 0);
        assert_eq!(container.safe_count(), 2);
    }

    /// A close for an unknown section id warns and continues. Other
    /// open sections stay open.
    #[test]
    fn section_close_with_unknown_id_leaves_others_open() {
        let mut container = ScrollbackContainer::new(0);
        let mut state = LiveBlocks::new();
        let kind = SectionKind::Markdown {
            source: Source::User,
        };

        state.append(&mut container, SectionId(1), kind, "hi".into());
        assert_eq!(container.active_count(), 1);

        assert!(!state.close(&mut container, SectionId(99), "test"));
        assert_eq!(container.active_count(), 1);
        assert_eq!(container.safe_count(), 0);
        assert_eq!(state.by_id.len(), 1);
    }
}
