use std::io::{Stdout, Write, stdout};

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
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Style};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::warn;

use frances_daemon::llm::Usage;
use frances_daemon::protocol::{
    self, ApprovalChoice, ApprovalKind, ApprovalRequest, BlockKind, DaemonStatus, StreamFrame,
};
use frances_daemon::session::Session;
use frances_tui::{
    BlockId as ContainerBlockId, InlineBackend, ScrollbackContainer, TruncatedBlock,
};

use crate::client;
use crate::tui::{FooterBlock, INPUT_HEIGHT, LabelledBlock, RawBlock, Textarea};

pub struct App<'a> {
    pub session: &'a Session,
    pub status: &'a DaemonStatus,
    /// The single daemon-to-client frame stream opened by
    /// [`crate::client::attach`]. The run loop spawns one reader task
    /// that pumps frames off it into the dispatch channel; initial
    /// scrollback, every prompt's frames, and any workflow-switch
    /// replays all arrive through this socket.
    pub events: UnixStream,
}

enum KeyAction {
    Quit,
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
    Ignore,
}

struct ActiveBlock {
    protocol_id: protocol::BlockId,
    container_id: ContainerBlockId,
    kind: BlockKind,
    text: String,
}

/// Local state machine for the in-progress streamed block. Out-of-order
/// frames (delta with no active block, stop for a mismatched id) become
/// explicit errors instead of silent drops.
enum BlockState {
    Idle,
    Active(ActiveBlock),
}

impl BlockState {
    fn new() -> Self {
        Self::Idle
    }

    /// Process one self-describing delta. If `self` is `Idle`, opens a
    /// new active block of `kind` with `text` as its initial content.
    /// If `self` is `Active` with a matching id, appends. If the id
    /// doesn't match, closes the previous block (returning its
    /// container id so the caller can `mark_safe` it) and opens a
    /// fresh one. Always leaves `self` in `Active` state.
    fn delta(
        &mut self,
        container: &mut ScrollbackContainer,
        id: protocol::BlockId,
        kind: BlockKind,
        text: &str,
    ) -> Option<ContainerBlockId> {
        if let Self::Active(active) = self
            && active.protocol_id == id
        {
            active.text.push_str(text);
            container.update_active(
                active.container_id,
                Box::new(LabelledBlock::new(active.kind.clone(), active.text.clone())),
            );
            return None;
        }
        // Either Idle, or Active with a different id. Take the previous
        // (if any) so the caller can mark_safe it, then open fresh.
        let previous = match std::mem::replace(self, Self::Idle) {
            Self::Active(prev) => Some(prev.container_id),
            Self::Idle => None,
        };
        let container_id =
            container.push_active(Box::new(LabelledBlock::new(kind.clone(), text.to_owned())));
        *self = Self::Active(ActiveBlock {
            protocol_id: id,
            container_id,
            kind,
            text: text.to_owned(),
        });
        previous
    }

    fn stop(&mut self, id: protocol::BlockId) -> Result<ContainerBlockId> {
        let container_id = match self {
            Self::Idle => {
                return Err(anyhow::anyhow!(
                    "BlockStop {id} arrived with no active block"
                ));
            }
            Self::Active(active) => {
                if active.protocol_id != id {
                    return Err(anyhow::anyhow!(
                        "BlockStop {id} does not match active block {}",
                        active.protocol_id
                    ));
                }
                active.container_id
            }
        };
        *self = Self::Idle;
        Ok(container_id)
    }

    /// Like [`stop`] but never returns an error: on an id mismatch
    /// or no-active condition (protocol bug on the daemon side), it
    /// logs via `tracing::warn!` and returns the orphan's container
    /// id (if any) so the caller can still `mark_safe` it. The
    /// dispatch loop uses this so a misbehaving daemon doesn't kill
    /// the TUI.
    ///
    /// `frame_label` is the wire frame's variant name, included in
    /// the log for triage.
    fn stop_or_recover(
        &mut self,
        id: protocol::BlockId,
        frame_label: &'static str,
    ) -> Option<ContainerBlockId> {
        match self.stop(id) {
            Ok(container_id) => Some(container_id),
            Err(err) => {
                warn!(%err, frame = frame_label, "protocol violation; recovering");
                self.take_container_id()
            }
        }
    }

    fn take_container_id(&mut self) -> Option<ContainerBlockId> {
        match std::mem::replace(self, Self::Idle) {
            Self::Active(active) => Some(active.container_id),
            Self::Idle => None,
        }
    }

    /// Drop any in-flight active tracking without writing back to the
    /// container. Used on `ScrollbackReset`: the container is about
    /// to be cleared, so the in-memory active entry will go away with
    /// it; the protocol tracking state on this side just needs to
    /// reset.
    fn discard(&mut self) {
        *self = Self::Idle;
    }
}

type AppTerminal = Terminal<InlineBackend<CrosstermBackend<Stdout>>>;

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

        let backend = InlineBackend::new(CrosstermBackend::new(stdout()), term_size);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )
        .context("init terminal")?;

        let mut container = ScrollbackContainer::new(
            Box::new(FooterBlock {
                textarea_lines: vec![String::new()],
                placeholder: String::new(),
                status: None,
            }),
            cursor_row,
        );
        for (i, line) in self.banner_lines().into_iter().enumerate() {
            let style = if i == 0 {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            };
            container.push(Box::new(RawBlock::single_styled(line, style)));
        }

        let mut textarea = Textarea::new("type a message…");
        let mut state = BlockState::new();
        let mut pending_approval: Option<ApprovalRequest> = None;
        let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<StreamFrame>();
        let mut events = EventStream::new();
        let mut streaming = false;
        let mut replay_mode = false;

        // Single reader task: pumps frames off the per-attach events
        // socket into `frame_rx`. The socket carries initial replay,
        // every prompt's frames, and any mid-cycle workflow-switch
        // replay. On EOF / read error (daemon gone, socket closed)
        // the task sends one final synthetic `Error` frame so the
        // user gets a visible "connection to daemon lost" row, then
        // exits. The dispatch loop keeps running — the user can
        // still close the TUI on their own time.
        let events_socket = self.events;
        let reader_tx = frame_tx.clone();
        tokio::spawn(async move {
            let mut stream = events_socket;
            loop {
                match client::next_event(&mut stream).await {
                    Ok(frame) => {
                        if reader_tx.send(frame).is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        let _ = reader_tx.send(StreamFrame::Error(format!(
                            "connection to daemon lost: {err}"
                        )));
                        return;
                    }
                }
            }
        });

        loop {
            redraw(&mut terminal, &mut container, &textarea, streaming)?;

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
                                ScrollbackAction::Ignore => {}
                            }
                        }
                        Event::Key(key) => {
                            if key.kind != KeyEventKind::Press { continue; }
                            match classify_key(&key, pending_approval.is_some()) {
                                KeyAction::Quit => return Ok(()),
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
                                    if textarea.is_empty() { continue; }
                                    let text = textarea.text();
                                    textarea.clear();
                                    if let Some(req) = pending_approval.take() {
                                        textarea.set_placeholder("type a message…");
                                        spawn_approval(
                                            self.session.clone(),
                                            req.id,
                                            ApprovalChoice::Chat { content: text },
                                            frame_tx.clone(),
                                        );
                                    } else {
                                        spawn_prompt(self.session.clone(), text, frame_tx.clone());
                                        streaming = true;
                                    }
                                }
                                KeyAction::Approve | KeyAction::Reject => {
                                    let Some(req) = pending_approval.take() else { continue; };
                                    let details = if textarea.is_empty() {
                                        None
                                    } else {
                                        Some(textarea.text())
                                    };
                                    textarea.clear();
                                    textarea.set_placeholder("type a message…");
                                    let choice = match classify_key(&key, true) {
                                        KeyAction::Approve => ApprovalChoice::Yes { details },
                                        _ => ApprovalChoice::No { details },
                                    };
                                    spawn_approval(
                                        self.session.clone(),
                                        req.id,
                                        choice,
                                        frame_tx.clone(),
                                    );
                                }
                                KeyAction::Edit => textarea.input(key),
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
                Some(frame) = frame_rx.recv() => {
                    // The transport-level boundary (`Done`) and any
                    // terminal frame (`Error`, `Approval`) end the
                    // streaming indicator. A `BlockDelta` re-enters
                    // streaming so a follow-up stream after an Error
                    // / Approval lights it back up. Replay frames
                    // don't change streaming state.
                    if !replay_mode {
                        match &frame {
                            StreamFrame::BlockDelta { .. } => streaming = true,
                            StreamFrame::Done
                            | StreamFrame::Error(_)
                            | StreamFrame::Approval(_) => streaming = false,
                            _ => {}
                        }
                    }
                    if let Some(req) = handle_frame(
                        &mut terminal,
                        &mut container,
                        &mut state,
                        &mut replay_mode,
                        frame,
                    )? {
                        textarea.set_placeholder(approval_placeholder(&req.kind));
                        pending_approval = Some(req);
                    }
                }
            }
        }
    }

    fn banner_lines(&self) -> Vec<String> {
        vec![
            format!("frances session {}", self.status.session_id),
            format!(
                "  daemon_pid={} protocol=v{:016x}",
                self.status.daemon_pid, self.status.protocol_version
            ),
            "  Enter to send. Alt+Enter for newline. Ctrl-O for history. Ctrl-C, Ctrl-D, or Esc to exit.".to_string(),
        ]
    }
}

fn redraw(
    terminal: &mut AppTerminal,
    container: &mut ScrollbackContainer,
    textarea: &Textarea,
    streaming: bool,
) -> std::io::Result<()> {
    container.set_footer(Box::new(FooterBlock {
        textarea_lines: textarea.lines_snapshot(),
        placeholder: textarea.placeholder().to_string(),
        status: if streaming {
            Some("streaming…".to_string())
        } else {
            None
        },
    }));

    if container.scrollback() {
        container.paint_scrollback(terminal)?;
        let backend = terminal.backend_mut();
        backend.hide_cursor()?;
        Backend::flush(backend)?;
        return Ok(());
    }

    container.draw(terminal)?;

    // Footer's first row sits at `footer_top_row` on the screen.
    // The textarea inside has a 1-row top border, then content rows,
    // then a 1-row bottom border — so the cursor goes at
    // footer_top + 1 (skip top border) + cursor_row (clamped to the
    // single visible content row of a 3-row textarea).
    let backend = terminal.backend_mut();
    let footer_top = container.footer_top_row();
    let content_rows = INPUT_HEIGHT.saturating_sub(2);
    let visible_cursor_row = textarea.cursor_row().min(content_rows.saturating_sub(1));
    let cursor_y = footer_top + 1 + visible_cursor_row;
    let cursor_x = 1 + textarea.cursor_display_col();
    backend.set_cursor_position(Position {
        x: cursor_x,
        y: cursor_y,
    })?;
    backend.show_cursor()?;
    Backend::flush(backend)?;
    Ok(())
}

/// Per-replay scratchpad: the same {id, kind, accumulated text}
/// triple the daemon uses, mirrored on the TUI side. Carried in
/// `App` state for the lifetime of one replay burst (between
/// `ScrollbackReset` and `ScrollbackReplayEnd`).
struct ReplayBlock {
    id: protocol::BlockId,
    kind: BlockKind,
    text: String,
}

fn handle_frame(
    terminal: &mut AppTerminal,
    container: &mut ScrollbackContainer,
    state: &mut BlockState,
    replay_mode: &mut bool,
    frame: StreamFrame,
) -> Result<Option<ApprovalRequest>> {
    if *replay_mode {
        return handle_replay_frame(terminal, container, replay_mode, frame);
    }
    match frame {
        StreamFrame::ScrollbackReset { .. } => {
            // Drop any in-flight live block tracking, then clear the
            // container — `clear()` force-spills current screen
            // content into native scrollback so the user keeps their
            // view in their terminal's history, and resets the
            // in-memory deques so the alt-screen inspector only shows
            // the new workflow's history.
            state.discard();
            container.clear(terminal)?;
            *replay_mode = true;
        }
        StreamFrame::ScrollbackReplayEnd => {
            // Defensive: replay end outside replay mode means a
            // malformed bracket. Ignore.
        }
        StreamFrame::BlockDelta { id, kind, text } => {
            if let Some(prev_id) = state.delta(container, id, kind, &text) {
                container.mark_safe(prev_id);
            }
        }
        StreamFrame::BlockStop { id } => {
            if let Some(container_id) = state.stop_or_recover(id, "BlockStop") {
                container.mark_safe(container_id);
            }
        }
        StreamFrame::BlockTruncated { id } => {
            // Live wire shouldn't emit truncated stops — they only
            // appear during replay. Recover the same way as a stop
            // for an unknown id (warn + mark orphan safe).
            if let Some(container_id) = state.stop_or_recover(id, "BlockTruncated") {
                container.mark_safe(container_id);
            }
        }
        StreamFrame::Usage(usage) => {
            container.push(Box::new(RawBlock::single_styled(
                format_usage(&usage),
                Style::default().fg(Color::DarkGray),
            )));
        }
        StreamFrame::Done => {
            // Done is a transport boundary ("this prompt's stream
            // ended"), not a semantic "close everything". Blocks live
            // until an explicit BlockStop or until a newer BlockStart
            // supersedes them.
        }
        StreamFrame::Error(message) => {
            if let Some(id) = state.take_container_id() {
                container.mark_safe(id);
            }
            container.push(Box::new(RawBlock::single_styled(
                format!("frances: error: {message}"),
                Style::default().fg(Color::Red),
            )));
        }
        StreamFrame::Approval(request) => {
            if let Some(id) = state.take_container_id() {
                container.mark_safe(id);
            }
            container.push(Box::new(RawBlock::single_styled(
                format!("approval: {}", request.prompt),
                Style::default().fg(Color::Yellow),
            )));
            return Ok(Some(request));
        }
    }
    Ok(None)
}

/// Frame handling while inside a replay bracket. Replayed blocks are
/// built up in a thread-local `ReplayBlock` accumulator and handed to
/// the container's `push_committed` on stop — they never touch the
/// live `active` deque.
fn handle_replay_frame(
    terminal: &mut AppTerminal,
    container: &mut ScrollbackContainer,
    replay_mode: &mut bool,
    frame: StreamFrame,
) -> Result<Option<ApprovalRequest>> {
    // Thread-local accumulator: the replay burst is single-threaded
    // per frame channel, so a `static mut` would do — but a refcell
    // in the function scope is uglier than tracking the in-flight
    // replay block via parameter wouldn't fit existing call sites.
    // Use a small bespoke shape via `static`:
    thread_local! {
        static REPLAY_OPEN: std::cell::RefCell<Option<ReplayBlock>> =
            const { std::cell::RefCell::new(None) };
    }
    match frame {
        StreamFrame::ScrollbackReset { .. } => {
            // Nested reset: rare, but treat as a fresh burst — clear
            // any half-built replay block and reset the container.
            REPLAY_OPEN.with(|cell| cell.borrow_mut().take());
            container.clear(terminal)?;
        }
        StreamFrame::ScrollbackReplayEnd => {
            // Any half-built block at end is just dropped; the daemon
            // is expected to close every block it opens.
            REPLAY_OPEN.with(|cell| cell.borrow_mut().take());
            *replay_mode = false;
        }
        StreamFrame::BlockDelta { id, kind, text } => {
            REPLAY_OPEN.with(|cell| {
                let mut guard = cell.borrow_mut();
                match guard.as_mut() {
                    Some(open) if open.id == id => {
                        open.text.push_str(&text);
                    }
                    _ => {
                        // Either nothing open, or a new id: drop any
                        // orphan (the daemon is expected to close
                        // every block it opens) and start fresh.
                        *guard = Some(ReplayBlock { id, kind, text });
                    }
                }
            });
        }
        StreamFrame::BlockStop { id } => {
            let finished = REPLAY_OPEN.with(|cell| {
                let mut guard = cell.borrow_mut();
                match guard.take() {
                    Some(open) if open.id == id => Some(open),
                    other => {
                        *guard = other;
                        None
                    }
                }
            });
            if let Some(open) = finished {
                container.push_committed(Box::new(LabelledBlock::new(open.kind, open.text)));
            }
        }
        StreamFrame::BlockTruncated { id } => {
            let finished = REPLAY_OPEN.with(|cell| {
                let mut guard = cell.borrow_mut();
                match guard.take() {
                    Some(open) if open.id == id => Some(open),
                    other => {
                        *guard = other;
                        None
                    }
                }
            });
            if let Some(open) = finished {
                let inner: Box<dyn frances_tui::Block> =
                    Box::new(LabelledBlock::new(open.kind, open.text));
                container.push_committed(Box::new(TruncatedBlock::new(inner)));
            }
        }
        StreamFrame::Error(message) => {
            container.push_committed(Box::new(RawBlock::single_styled(
                format!("frances: error: {message}"),
                Style::default().fg(Color::Red),
            )));
        }
        // Usage / Done / Approval are not part of the replay burst.
        // Drop them if they sneak in.
        StreamFrame::Usage(_) | StreamFrame::Done | StreamFrame::Approval(_) => {}
    }
    Ok(None)
}

fn approval_placeholder(kind: &ApprovalKind) -> &'static str {
    match kind {
        ApprovalKind::YesNo => "Alt+Y yes  Alt+N no  Enter chat (text becomes details for yes/no)",
    }
}

fn spawn_prompt(session: Session, prompt: String, frame_tx: mpsc::UnboundedSender<StreamFrame>) {
    tokio::spawn(async move {
        if let Err(error) = client::prompt(&session, prompt).await {
            let _ = frame_tx.send(StreamFrame::Error(format!("{error:#}")));
            let _ = frame_tx.send(StreamFrame::Done);
        }
    });
}

fn spawn_approval(
    session: Session,
    id: frances_daemon::protocol::ApprovalId,
    choice: ApprovalChoice,
    frame_tx: mpsc::UnboundedSender<StreamFrame>,
) {
    tokio::spawn(async move {
        if let Err(error) = client::respond_approval(&session, id, choice).await {
            let _ = frame_tx.send(StreamFrame::Error(format!("approval: {error:#}")));
        }
    });
}

fn format_usage(usage: &Usage) -> String {
    format!(
        "  ↳ tokens: prompt={} (cached={}) completion={} total={}",
        usage.prompt_tokens, usage.cached_input_tokens, usage.completion_tokens, usage.total_tokens
    )
}

fn classify_key(key: &KeyEvent, pending_approval: bool) -> KeyAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Esc => KeyAction::Quit,
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
        _ => ScrollbackAction::Ignore,
    }
}

/// Rows to scroll on PageUp / PageDown. Leaves a 1-row anchor of
/// visible content above/below the new window so the user can see
/// where they came from.
fn scrollback_page(terminal_h: u16) -> u16 {
    // Content area = terminal_h - 2 status bars - footer (typically 3-row textarea).
    terminal_h.saturating_sub(6).max(1)
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
    use frances_tui::Block;
    use ratatui::text::Line;
    use ratatui::widgets::Paragraph;

    fn footer() -> Box<dyn Block> {
        Box::new(Paragraph::new(Line::raw("footer")))
    }

    /// Drives `BlockState::delta` against a real `ScrollbackContainer`
    /// (paragraph footer; no terminal needed). Verifies the
    /// delta-driven state machine: a first delta on `Idle` opens the
    /// block; same-id subsequent delta appends; different-id delta
    /// closes the previous one (returning its container id so the
    /// caller can `mark_safe`) and opens a fresh block.
    #[test]
    fn block_state_delta_opens_appends_and_supersedes() {
        let mut container = ScrollbackContainer::new(footer(), 0);
        let mut state = BlockState::new();
        let kind_a = BlockKind::Text {
            sender: Some("user".into()),
        };
        let kind_b = BlockKind::ToolUse {
            name: "shell".into(),
        };

        // First delta on Idle: opens A. No previous container id to
        // mark safe.
        assert!(
            state
                .delta(&mut container, protocol::BlockId(1), kind_a.clone(), "hel")
                .is_none()
        );
        assert_eq!(container.active_count(), 1);
        assert_eq!(container.safe_count(), 0);

        // Same-id delta: appends. Still no previous id returned.
        assert!(
            state
                .delta(&mut container, protocol::BlockId(1), kind_a.clone(), "lo")
                .is_none()
        );
        assert_eq!(container.active_count(), 1);

        // Different-id delta: closes A (returns its container id so
        // caller can mark_safe) and opens B.
        let prev = state.delta(&mut container, protocol::BlockId(2), kind_b.clone(), "ls");
        let prev_id = prev.expect("supersession returns prior container id");
        container.mark_safe(prev_id);
        // A drained to safe, B is the new active.
        assert_eq!(container.active_count(), 1);
        assert_eq!(container.safe_count(), 1);

        // Closing B leaves both in safe / nothing active.
        let b_id = state.stop(protocol::BlockId(2)).expect("stop B");
        container.mark_safe(b_id);
        assert_eq!(container.active_count(), 0);
        assert_eq!(container.safe_count(), 2);
    }

    /// `BlockStop` for an id we don't have open is a protocol bug
    /// on the daemon side. The TUI should not bail back to the CLI
    /// — it should log and continue. With no active block at all,
    /// `stop_or_recover` returns `None` so the caller does nothing.
    #[test]
    fn block_stop_for_unknown_id_does_not_bail() {
        let container = ScrollbackContainer::new(footer(), 0);
        let mut state = BlockState::new();

        let result = state.stop_or_recover(protocol::BlockId(99), "BlockStop");

        assert!(
            result.is_none(),
            "with no active block there's nothing to mark safe"
        );
        assert_eq!(container.active_count(), 0);
        assert_eq!(container.safe_count(), 0);
        assert_eq!(container.committed_count(), 0);
    }

    /// `BlockStop` whose id doesn't match the active block. The
    /// orphan active block must still be reclaimed (via the
    /// returned container id, which the caller marks safe), so the
    /// container doesn't drift into a half-open state.
    #[test]
    fn block_stop_with_mismatched_id_marks_orphan_safe_and_recovers() {
        let mut container = ScrollbackContainer::new(footer(), 0);
        let mut state = BlockState::new();
        let kind = BlockKind::Text {
            sender: Some("user".into()),
        };

        // Open id 1.
        let _ = state.delta(&mut container, protocol::BlockId(1), kind, "hi");
        assert_eq!(container.active_count(), 1);

        // Stop with id 2 — mismatched.
        let recovered = state.stop_or_recover(protocol::BlockId(2), "BlockStop");
        let orphan = recovered.expect("mismatch path returns orphan's container id");
        container.mark_safe(orphan);

        // Orphan drained to safe; nothing left active; the state
        // machine is back to Idle.
        assert_eq!(container.active_count(), 0);
        assert_eq!(container.safe_count(), 1);
        assert!(matches!(state, BlockState::Idle));
    }
}
