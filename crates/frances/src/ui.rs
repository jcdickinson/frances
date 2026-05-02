use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};
use ratatui::text::Text;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::runtime::Handle;
use tui_textarea::TextArea;

use crate::daemon::client;
use crate::daemon::protocol::{DaemonStatus, PromptId, StreamFrame};
use crate::llm::Usage;
use crate::session::Session;

static NEXT_PROMPT_ID: AtomicU64 = AtomicU64::new(1);

fn next_prompt_id() -> PromptId {
    NEXT_PROMPT_ID.fetch_add(1, Ordering::Relaxed)
}

const INPUT_VIEWPORT_HEIGHT: u16 = 3;

pub struct App<'a> {
    pub session: &'a Session,
    pub status: &'a DaemonStatus,
}

enum KeyAction {
    Quit,
    Submit,
    Edit,
}

impl App<'_> {
    pub fn run(self) -> Result<()> {
        enable_raw_mode()?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(INPUT_VIEWPORT_HEIGHT),
            },
        )?;

        let result = self.run_loop(&mut terminal);

        let _ = terminal.clear();
        let _ = disable_raw_mode();
        let _ = terminal.show_cursor();
        println!();

        result
    }

    fn run_loop(self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        emit_lines(terminal, &self.banner_lines())?;

        let mut textarea = build_textarea();

        loop {
            terminal.draw(|frame| frame.render_widget(&textarea, frame.area()))?;

            if !event::poll(Duration::from_millis(100))? {
                continue;
            }

            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match classify_key(&key) {
                KeyAction::Quit => return Ok(()),
                KeyAction::Submit => {
                    if textarea.is_empty() {
                        continue;
                    }
                    let prompt = textarea.lines().join("\n");
                    textarea = build_textarea();

                    let echoed: Vec<String> =
                        prompt.lines().map(|line| format!("you: {line}")).collect();
                    emit_lines(terminal, &echoed)?;

                    match run_streaming(terminal, self.session, prompt)? {
                        StreamOutcome::Quit => return Ok(()),
                        StreamOutcome::Done { response, usage } => {
                            if !response.is_empty() {
                                let lines: Vec<String> = response
                                    .lines()
                                    .map(|line| format!("frances: {line}"))
                                    .collect();
                                emit_lines(terminal, &lines)?;
                            }
                            if let Some(usage) = usage {
                                emit_lines(terminal, &[format_usage(&usage)])?;
                            }
                        }
                        StreamOutcome::Error(message) => {
                            emit_lines(terminal, &[format!("frances: error: {message}")])?;
                        }
                    }
                }
                KeyAction::Edit => {
                    textarea.input(key);
                }
            }
        }
    }

    fn banner_lines(&self) -> Vec<String> {
        vec![
            format!("frances session {}", self.status.session_id),
            format!(
                "  daemon_pid={} protocol=v{}",
                self.status.daemon_pid, self.status.protocol_version
            ),
            "  Enter to send. Alt+Enter for newline. Ctrl-C, Ctrl-D, or Esc to exit.".to_string(),
        ]
    }
}

enum StreamOutcome {
    Done {
        response: String,
        usage: Option<Usage>,
    },
    Error(String),
    Quit,
}

fn format_usage(usage: &Usage) -> String {
    let cached = usage
        .prompt_tokens_details
        .as_ref()
        .map(|d| d.cached_tokens)
        .unwrap_or(0);
    format!(
        "  ↳ tokens: prompt={} (cached={}) completion={} total={}",
        usage.prompt_tokens, cached, usage.completion_tokens, usage.total_tokens
    )
}

fn run_streaming(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    session: &Session,
    prompt: String,
) -> Result<StreamOutcome> {
    let (tx, rx) = mpsc::channel::<StreamFrame>();
    let session_clone = session.clone();
    let tx_clone = tx.clone();
    let handle = Handle::current();
    let prompt_id = next_prompt_id();
    thread::spawn(move || {
        let result = handle.block_on(client::prompt_stream(
            &session_clone,
            prompt_id,
            prompt,
            |frame| {
                let _ = tx_clone.send(frame);
            },
        ));
        if let Err(error) = result {
            let _ = tx.send(StreamFrame::Error(format!("{error:#}")));
            let _ = tx.send(StreamFrame::Done);
        }
    });

    let mut accumulated = String::new();
    let mut error: Option<String> = None;
    let mut usage: Option<Usage> = None;

    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let block = Block::default()
                .borders(Borders::ALL)
                .title("frances")
                .title_style(Style::default().add_modifier(Modifier::BOLD));
            let body = if accumulated.is_empty() {
                "thinking…".to_string()
            } else {
                accumulated.clone()
            };
            let para = Paragraph::new(Text::raw(body))
                .block(block)
                .wrap(Wrap { trim: false });
            frame.render_widget(para, area);
        })?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && matches!(classify_key(&key), KeyAction::Quit)
                {
                    return Ok(StreamOutcome::Quit);
                }
            }
        }

        loop {
            match rx.try_recv() {
                Ok(StreamFrame::Text(text)) => accumulated.push_str(&text),
                Ok(StreamFrame::Usage(received)) => {
                    usage = Some(received);
                }
                Ok(StreamFrame::Done) => {
                    return Ok(match error {
                        Some(message) => StreamOutcome::Error(message),
                        None => StreamOutcome::Done {
                            response: accumulated,
                            usage,
                        },
                    });
                }
                Ok(StreamFrame::Error(message)) => {
                    error = Some(message);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Ok(match error {
                        Some(message) => StreamOutcome::Error(message),
                        None => StreamOutcome::Error("stream ended without response".to_string()),
                    });
                }
            }
        }
    }
}

fn classify_key(key: &KeyEvent) -> KeyAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Esc => KeyAction::Quit,
        KeyCode::Char('c' | 'd') if ctrl => KeyAction::Quit,
        KeyCode::Enter if !alt => KeyAction::Submit,
        _ => KeyAction::Edit,
    }
}

fn build_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_block(
        Block::default()
            .borders(Borders::ALL)
            .title("frances")
            .title_style(Style::default().add_modifier(Modifier::BOLD)),
    );
    ta.set_placeholder_text("type a message…");
    ta.set_cursor_line_style(Style::default());
    ta
}

fn emit_lines(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    lines: &[String],
) -> io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let height = lines.len() as u16;
    terminal.insert_before(height, |buf: &mut Buffer| {
        for (idx, line) in lines.iter().enumerate() {
            buf.set_string(0, idx as u16, line, Style::default());
        }
    })
}
