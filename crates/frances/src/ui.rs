use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tui_textarea::TextArea;

use crate::daemon::protocol::DaemonStatus;

const INPUT_VIEWPORT_HEIGHT: u16 = 3;

pub struct App<'a> {
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
                    let lines = textarea.lines().to_vec();
                    textarea = build_textarea();
                    let echoed: Vec<String> =
                        lines.iter().map(|line| format!("you: {line}")).collect();
                    emit_lines(terminal, &echoed)?;
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
