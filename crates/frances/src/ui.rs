use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use crossterm::QueueableCommand;
use crossterm::cursor::MoveTo;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::daemon::client;
use crate::daemon::protocol::{BlockKind, DaemonStatus, PromptId, StreamFrame};
use crate::llm::Usage;
use crate::session::Session;
use crate::tui::region::Region;
use crate::tui::textarea::INPUT_HEIGHT;
use crate::tui::widget::Widget;
use crate::tui::{BlockView, Screen, Textarea, scrollback};

static NEXT_PROMPT_ID: AtomicU64 = AtomicU64::new(1);

fn next_prompt_id() -> PromptId {
    NEXT_PROMPT_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct App<'a> {
    pub session: &'a Session,
    pub status: &'a DaemonStatus,
}

enum KeyAction {
    Quit,
    Submit,
    Edit,
}

struct Active {
    id: u64,
    kind: BlockKind,
    text: String,
    committed_lines: u16,
}

impl App<'_> {
    pub async fn run(self) -> Result<()> {
        let mut screen = Screen::new().context("init screen")?;

        let outcome = self.run_loop(&mut screen).await;

        let _ = screen.shutdown();
        outcome
    }

    async fn run_loop(self, screen: &mut Screen) -> Result<()> {
        scrollback::emit_text(screen, &self.banner_lines())?;

        let mut textarea = Textarea::new("type a message…");
        let mut active: Option<Active> = None;
        let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<StreamFrame>();
        let mut events = EventStream::new();

        loop {
            redraw(screen, &textarea, &mut active)?;

            tokio::select! {
                Some(event) = events.next() => {
                    let event = event.context("event read")?;
                    match event {
                        Event::Key(key) => {
                            if key.kind != KeyEventKind::Press { continue; }
                            match classify_key(&key) {
                                KeyAction::Quit => return Ok(()),
                                KeyAction::Submit => {
                                    if textarea.is_empty() { continue; }
                                    let prompt = textarea.text();
                                    textarea.clear();
                                    spawn_stream(self.session.clone(), prompt, frame_tx.clone());
                                }
                                KeyAction::Edit => textarea.input(key),
                            }
                        }
                        Event::Resize(width, height) => {
                            screen.handle_resize(width, height);
                        }
                        _ => {}
                    }
                }
                Some(frame) = frame_rx.recv() => {
                    handle_frame(screen, &mut active, frame)?;
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
            "  Enter to send. Alt+Enter for newline. Ctrl-C, Ctrl-D, or Esc to exit.".to_string(),
        ]
    }
}

fn redraw(screen: &mut Screen, textarea: &Textarea, active: &mut Option<Active>) -> Result<()> {
    let width = screen.width();
    let max_block_rows = screen.height().saturating_sub(INPUT_HEIGHT);

    let visible_lines: Vec<String> = if let Some(a) = active.as_mut() {
        let wrapped = BlockView::new(a.kind, &a.text).wrapped_lines(width);
        let total = wrapped.len() as u16;
        let uncommitted = total.saturating_sub(a.committed_lines);
        let overflow = uncommitted.saturating_sub(max_block_rows);
        if overflow > 0 {
            let start = a.committed_lines as usize;
            let end = start + overflow as usize;
            screen.emit_above(&wrapped[start..end])?;
            a.committed_lines += overflow;
        }
        wrapped[a.committed_lines as usize..].to_vec()
    } else {
        Vec::new()
    };

    let block_height = visible_lines.len() as u16;
    let target = block_height.saturating_add(INPUT_HEIGHT);
    screen.set_viewport_height(target)?;

    screen.draw_frame(|ctx| {
        for (i, line) in visible_lines.iter().enumerate() {
            ctx.stdout.queue(MoveTo(0, ctx.viewport_top + i as u16))?;
            ctx.stdout.queue(Print(line))?;
            ctx.stdout.queue(Clear(ClearType::UntilNewLine))?;
        }
        let input_region = Region {
            x: 0,
            y: ctx.viewport_top + block_height,
            width: ctx.viewport_width,
            height: INPUT_HEIGHT,
        };
        textarea.render(input_region, ctx)?;
        Ok(())
    })?;

    Ok(())
}

fn handle_frame(
    screen: &mut Screen,
    active: &mut Option<Active>,
    frame: StreamFrame,
) -> Result<()> {
    match frame {
        StreamFrame::BlockStart { id, kind } => {
            if let Some(prev) = active.take() {
                commit_remaining(screen, &prev)?;
            }
            *active = Some(Active {
                id,
                kind,
                text: String::new(),
                committed_lines: 0,
            });
        }
        StreamFrame::BlockDelta { id, text } => {
            if let Some(a) = active.as_mut() {
                if a.id == id {
                    a.text.push_str(&text);
                }
            }
        }
        StreamFrame::BlockStop { id } => {
            if let Some(a) = active.as_ref() {
                if a.id == id {
                    let prev = active.take().expect("checked above");
                    commit_remaining(screen, &prev)?;
                }
            }
        }
        StreamFrame::Usage(usage) => {
            scrollback::emit_text(screen, &[format_usage(&usage)])?;
        }
        StreamFrame::Done => {
            if let Some(prev) = active.take() {
                commit_remaining(screen, &prev)?;
            }
        }
        StreamFrame::Error(message) => {
            if let Some(prev) = active.take() {
                commit_remaining(screen, &prev)?;
            }
            scrollback::emit_text(screen, &[format!("frances: error: {message}")])?;
        }
    }
    Ok(())
}

fn commit_remaining(screen: &mut Screen, active: &Active) -> io::Result<()> {
    // The visible portion of the active block is already painted into the top
    // of the viewport by the most recent redraw. To finalise it into scrollback
    // we just shrink the viewport by that many rows — the bytes stay where
    // they are on screen but stop being repainted. Doing it this way avoids
    // `emit_above`'s scroll-and-rewrite, which would otherwise displace the
    // textarea borders into the rows we're about to drop into scrollback.
    let wrapped = BlockView::new(active.kind, &active.text).wrapped_lines(screen.width());
    let uncommitted = (wrapped.len() as u16).saturating_sub(active.committed_lines);
    if uncommitted > 0 {
        let new_height = screen.viewport_height().saturating_sub(uncommitted);
        screen.set_viewport_height(new_height)?;
    }
    Ok(())
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

fn format_usage(usage: &Usage) -> String {
    format!(
        "  ↳ tokens: prompt={} (cached={}) completion={} total={}",
        usage.prompt_tokens, usage.cached_input_tokens, usage.completion_tokens, usage.total_tokens
    )
}

fn classify_key(key: &KeyEvent) -> KeyAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Esc => KeyAction::Quit,
        KeyCode::Char('c' | 'd') if ctrl => KeyAction::Quit,
        KeyCode::Enter if !alt => KeyAction::Submit,
        _ => {
            let _ = (ctrl, alt);
            KeyAction::Edit
        }
    }
}
