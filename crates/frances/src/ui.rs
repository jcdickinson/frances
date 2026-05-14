use std::io::{Stdout, Write, stdout};
use std::sync::atomic::{AtomicU64, Ordering};

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
use tokio::sync::mpsc;

use frances_daemon::llm::Usage;
use frances_daemon::protocol::{
    self, ApprovalChoice, ApprovalKind, ApprovalRequest, BlockKind, DaemonStatus, PromptId,
    StreamFrame,
};
use frances_daemon::session::Session;
use frances_tui::{BlockId as ContainerBlockId, InlineBackend, ScrollbackContainer};

use crate::client;
use crate::tui::{FooterBlock, INPUT_HEIGHT, LabelledBlock, RawBlock, Textarea};

static NEXT_PROMPT_ID: AtomicU64 = AtomicU64::new(1);

fn next_prompt_id() -> PromptId {
    PromptId(NEXT_PROMPT_ID.fetch_add(1, Ordering::Relaxed))
}

pub struct App<'a> {
    pub session: &'a Session,
    pub status: &'a DaemonStatus,
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

    /// Start a new active block in the container and remember the
    /// returned id. If a block was already active, returns its
    /// container id so the caller can `mark_safe` it.
    fn start(
        &mut self,
        container: &mut ScrollbackContainer,
        id: protocol::BlockId,
        kind: BlockKind,
    ) -> Option<ContainerBlockId> {
        let previous = match std::mem::replace(self, Self::Idle) {
            Self::Active(prev) => Some(prev.container_id),
            Self::Idle => None,
        };
        let container_id =
            container.push_active(Box::new(LabelledBlock::new(kind.clone(), String::new())));
        *self = Self::Active(ActiveBlock {
            protocol_id: id,
            container_id,
            kind,
            text: String::new(),
        });
        previous
    }

    fn delta(
        &mut self,
        container: &mut ScrollbackContainer,
        id: protocol::BlockId,
        text: &str,
    ) -> Result<()> {
        let active = match self {
            Self::Idle => {
                return Err(anyhow::anyhow!(
                    "BlockDelta {id} arrived with no active block"
                ));
            }
            Self::Active(active) => active,
        };
        if active.protocol_id != id {
            return Err(anyhow::anyhow!(
                "BlockDelta {id} does not match active block {}",
                active.protocol_id
            ));
        }
        active.text.push_str(text);
        container.update_active(
            active.container_id,
            Box::new(LabelledBlock::new(active.kind.clone(), active.text.clone())),
        );
        Ok(())
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

    fn take_container_id(&mut self) -> Option<ContainerBlockId> {
        match std::mem::replace(self, Self::Idle) {
            Self::Active(active) => Some(active.container_id),
            Self::Idle => None,
        }
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
                                    container.set_scrollback(false);
                                    leave_scrollback(&mut terminal)?;
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
                                    container.set_scrollback(true);
                                    enter_scrollback(&mut terminal)?;
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
                                        spawn_stream(self.session.clone(), text, frame_tx.clone());
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
                            terminal
                                .backend_mut()
                                .handle_terminal_resize(term_size)?;
                            terminal.clear()?;
                        }
                        _ => {}
                    }
                }
                Some(frame) = frame_rx.recv() => {
                    // The transport-level boundary (`Done`) and any
                    // terminal frame (`Error`, `Approval`) end the
                    // streaming indicator. `BlockStart` re-enters
                    // streaming so a follow-up stream after an Error
                    // / Approval lights it back up.
                    match &frame {
                        StreamFrame::BlockStart { .. } => streaming = true,
                        StreamFrame::Done
                        | StreamFrame::Error(_)
                        | StreamFrame::Approval(_) => streaming = false,
                        _ => {}
                    }
                    if let Some(req) = handle_frame(
                        &mut container,
                        &mut state,
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

fn handle_frame(
    container: &mut ScrollbackContainer,
    state: &mut BlockState,
    frame: StreamFrame,
) -> Result<Option<ApprovalRequest>> {
    match frame {
        StreamFrame::BlockStart { id, kind } => {
            if let Some(prev_id) = state.start(container, id, kind) {
                container.mark_safe(prev_id);
            }
        }
        StreamFrame::BlockDelta { id, text } => {
            state.delta(container, id, &text)?;
        }
        StreamFrame::BlockStop { id } => {
            let container_id = state.stop(id)?;
            container.mark_safe(container_id);
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

fn approval_placeholder(kind: &ApprovalKind) -> &'static str {
    match kind {
        ApprovalKind::YesNo => "Alt+Y yes  Alt+N no  Enter chat (text becomes details for yes/no)",
    }
}

fn spawn_stream(session: Session, prompt: String, frame_tx: mpsc::UnboundedSender<StreamFrame>) {
    let prompt_id = next_prompt_id();
    tokio::spawn(async move {
        let result = client::prompt_stream(&session, prompt_id, prompt, |frame| {
            let _ = frame_tx.send(frame);
        })
        .await;
        if let Err(error) = result {
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
