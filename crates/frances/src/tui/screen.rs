use std::io::{self, Stdout, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::Print;
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, disable_raw_mode, enable_raw_mode, size,
};
use crossterm::{QueueableCommand, queue};

use super::widget::RenderCtx;

pub struct Screen {
    stdout: Stdout,
    width: u16,
    height: u16,
    viewport_height: u16,
}

impl Screen {
    pub fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let (width, height) = size()?;
        Ok(Self {
            stdout: io::stdout(),
            width,
            height,
            viewport_height: 0,
        })
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        let bottom = self.height.saturating_sub(1);
        self.stdout.queue(MoveTo(0, bottom))?;
        self.stdout.queue(Show)?;
        self.stdout.flush()?;
        disable_raw_mode()?;
        println!();
        Ok(())
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn viewport_height(&self) -> u16 {
        self.viewport_height
    }

    #[expect(
        dead_code,
        reason = "viewport math accessor; will be used once scrollback rendering is wired"
    )]
    pub fn viewport_top(&self) -> u16 {
        self.height.saturating_sub(self.viewport_height)
    }

    pub fn handle_resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    pub fn set_viewport_height(&mut self, target: u16) -> io::Result<()> {
        let target = target.min(self.height);
        if target > self.viewport_height {
            let delta = target - self.viewport_height;
            // Park cursor at the very bottom row, then write `delta` newlines.
            // Each `\n` at the last visible row scrolls the terminal up by 1
            // (existing content flows into native scrollback) and the cursor
            // stays on the (new) last row.
            let bottom = self.height.saturating_sub(1);
            self.stdout.queue(MoveTo(0, bottom))?;
            for _ in 0..delta {
                self.stdout.queue(Print("\n"))?;
            }
            self.stdout.flush()?;
        }
        // Shrinking is just a counter update — the rows that were viewport
        // become scrollback in place and we simply stop redrawing them. The
        // caller is responsible for ensuring those rows hold the content
        // they want to leave behind in scrollback.
        self.viewport_height = target;
        Ok(())
    }

    pub fn draw_frame<F>(&mut self, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut RenderCtx) -> io::Result<()>,
    {
        queue!(self.stdout, BeginSynchronizedUpdate, Hide)?;
        let mut ctx = RenderCtx {
            stdout: &mut self.stdout,
            viewport_top: self.height.saturating_sub(self.viewport_height),
            viewport_width: self.width,
            viewport_height: self.viewport_height,
        };
        render(&mut ctx)?;
        queue!(self.stdout, Show, EndSynchronizedUpdate)?;
        self.stdout.flush()
    }

    /// Insert `lines` into native scrollback above the current viewport.
    /// The viewport stays anchored at the terminal bottom.
    pub fn emit_above(&mut self, lines: &[String]) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        // Strategy: park cursor at the very bottom, write `lines.len()`
        // newlines (scrolls terminal up by N), then position cursor at the
        // top of the (newly grown) scrollback rows just above the viewport
        // and write each line. Finally, force a redraw of viewport contents
        // by leaving the cursor at viewport_top — caller's next draw_frame
        // will repaint over the displaced rows.
        let n = lines.len() as u16;
        let bottom = self.height.saturating_sub(1);
        self.stdout.queue(MoveTo(0, bottom))?;
        for _ in 0..n {
            self.stdout.queue(Print("\n"))?;
        }
        // After the scroll, the rows that USED to be the top N rows of the
        // viewport are now sitting just above the (still-anchored-to-bottom)
        // viewport. Their contents got pushed up but we want our `lines`
        // there instead. Position at the start of those rows and overwrite.
        let scrollback_top = self.height.saturating_sub(self.viewport_height + n);
        for (i, line) in lines.iter().enumerate() {
            self.stdout.queue(MoveTo(0, scrollback_top + i as u16))?;
            self.stdout.queue(Print(line))?;
            self.stdout.queue(crossterm::terminal::Clear(
                crossterm::terminal::ClearType::UntilNewLine,
            ))?;
        }
        self.stdout.flush()
    }
}
