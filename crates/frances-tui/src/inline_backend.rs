//! [`InlineBackend`] — a thin `ratatui::backend::Backend` wrapper that
//! is **transparent**: it doesn't track or translate a top-row /
//! content-height of its own. The `ScrollbackContainer` drives the
//! terminal directly via the byte stream (cursor positioning + row
//! writes + `\n`s) and lets the terminal's own scroll behaviour push
//! old content into native scrollback.
//!
//! The wrapper still exists because:
//!
//! * It holds the terminal size that the container needs every frame.
//! * It provides crate-private row-level helpers — [`InlineBackend::move_cursor_abs`],
//!   [`InlineBackend::write_row`], [`InlineBackend::newline`] — that
//!   the container uses to emit rows with styling and the natural-
//!   scroll `\r\n` between them.
//! * It hosts [`SyncGuard`] so a whole frame composites atomically
//!   via DEC mode 2026 synchronised output.

use std::io::{self, Write};

use crossterm::QueueableCommand;
use crossterm::cursor::MoveTo;
use crossterm::style::Print;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

pub struct InlineBackend<B: Backend<Error = io::Error> + Write> {
    inner: B,
    terminal_size: Size,
}

impl<B: Backend<Error = io::Error> + Write> InlineBackend<B> {
    /// Construct an InlineBackend over `inner`. The container's cursor
    /// origin is the caller's responsibility — pass it to
    /// `ScrollbackContainer::new`.
    pub fn new(inner: B, terminal_size: Size) -> Self {
        Self {
            inner,
            terminal_size,
        }
    }

    pub fn terminal_size(&self) -> Size {
        self.terminal_size
    }

    /// Crate-private peek at the underlying backend, used by tests
    /// to introspect a simulated terminal state after a draw.
    #[cfg(test)]
    pub(crate) fn inner(&self) -> &B {
        &self.inner
    }

    /// Move the cursor to an absolute screen position.
    pub(crate) fn move_cursor_abs(&mut self, x: u16, y: u16) -> io::Result<()> {
        self.inner.queue(MoveTo(x, y))?;
        Ok(())
    }

    /// Write one terminal row's worth of cells at the current cursor
    /// position, applying each cell's ANSI styling. Does **not** emit
    /// a trailing newline — the caller controls that via [`newline`].
    pub(crate) fn write_row<'a, I>(&mut self, cells: I) -> io::Result<()>
    where
        I: Iterator<Item = &'a Cell>,
    {
        use crossterm::style::{Print, ResetColor, SetBackgroundColor, SetForegroundColor};
        for cell in cells {
            self.inner
                .queue(SetForegroundColor(to_crossterm_color(cell.fg)))?;
            self.inner
                .queue(SetBackgroundColor(to_crossterm_color(cell.bg)))?;
            self.inner.queue(Print(cell.symbol()))?;
        }
        self.inner.queue(ResetColor)?;
        Ok(())
    }

    /// Move the cursor to `(x, y)` and write a single styled cell.
    /// Used by the container's footer diff path to emit only the
    /// cells that changed since the previous frame — i.e. ratatui's
    /// `Buffer::diff` output, one item at a time.
    pub(crate) fn write_cell(&mut self, x: u16, y: u16, cell: &Cell) -> io::Result<()> {
        use crossterm::style::{ResetColor, SetBackgroundColor, SetForegroundColor};
        self.inner.queue(MoveTo(x, y))?;
        self.inner
            .queue(SetForegroundColor(to_crossterm_color(cell.fg)))?;
        self.inner
            .queue(SetBackgroundColor(to_crossterm_color(cell.bg)))?;
        self.inner.queue(Print(cell.symbol()))?;
        self.inner.queue(ResetColor)?;
        Ok(())
    }

    /// Emit `\r\n`. The `\r` returns the cursor to column 0 (in raw
    /// mode `\n` alone doesn't); the `\n` advances to the next row,
    /// or scrolls the screen up by 1 if the cursor is already on the
    /// last row — that's the native-scrollback push.
    pub(crate) fn newline(&mut self) -> io::Result<()> {
        self.inner.queue(Print("\r\n"))?;
        Ok(())
    }

    /// Move cursor to the start of screen row `y` and clear that
    /// entire line. Used by the container to wipe rows the previous
    /// footer occupied when the new footer is shorter — they're
    /// being yielded back to the terminal, so they must be blank
    /// rather than stale.
    pub(crate) fn clear_line(&mut self, y: u16) -> io::Result<()> {
        use crossterm::terminal::{Clear, ClearType as CtClearType};
        self.inner.queue(MoveTo(0, y))?;
        self.inner.queue(Clear(CtClearType::CurrentLine))?;
        Ok(())
    }

    /// React to a terminal-size change.
    pub fn handle_terminal_resize(&mut self, new_size: Size) -> io::Result<()> {
        self.terminal_size = new_size;
        Ok(())
    }

    /// Move the cursor home and erase from there to the end of the
    /// display. Used by the container when leaving active-overflow
    /// mode — leftover ellipsis + truncated paint must be wiped before
    /// the natural-scroll path can take over with a fresh canvas.
    ///
    /// Uses `CSI H` + `CSI J` rather than `CSI 2 J` because the latter
    /// is interpreted by some terminals (notably Alacritty) as "scroll
    /// the visible screen into scrollback before clearing" — exactly
    /// the leak we're trying to avoid.
    pub(crate) fn clear_below_home(&mut self) -> io::Result<()> {
        self.inner.write_all(b"\x1b[H\x1b[J")?;
        Ok(())
    }
}

fn to_crossterm_color(c: ratatui::style::Color) -> crossterm::style::Color {
    use crossterm::style::Color as CC;
    use ratatui::style::Color as RC;
    match c {
        RC::Reset => CC::Reset,
        RC::Black => CC::Black,
        RC::Red => CC::DarkRed,
        RC::Green => CC::DarkGreen,
        RC::Yellow => CC::DarkYellow,
        RC::Blue => CC::DarkBlue,
        RC::Magenta => CC::DarkMagenta,
        RC::Cyan => CC::DarkCyan,
        RC::Gray => CC::Grey,
        RC::DarkGray => CC::DarkGrey,
        RC::LightRed => CC::Red,
        RC::LightGreen => CC::Green,
        RC::LightYellow => CC::Yellow,
        RC::LightBlue => CC::Blue,
        RC::LightMagenta => CC::Magenta,
        RC::LightCyan => CC::Cyan,
        RC::White => CC::White,
        RC::Rgb(r, g, b) => CC::Rgb { r, g, b },
        RC::Indexed(i) => CC::AnsiValue(i),
    }
}

impl<B: Backend<Error = io::Error> + Write> Write for InlineBackend<B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

// `ScrollbackContainer` does not use ratatui's `terminal.draw` path —
// it writes rows + `\n`s directly via the helpers above — but
// `Terminal::with_options` still requires `Backend`, so we provide a
// transparent implementation that just delegates to the inner backend.
impl<B: Backend<Error = io::Error> + Write> Backend for InlineBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        Ok(self.terminal_size)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        let mut ws = self.inner.window_size()?;
        ws.columns_rows = self.terminal_size;
        Ok(ws)
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        Backend::flush(&mut self.inner)
    }
}

/// RAII guard that opens a DEC mode 2026 synchronized-output region on
/// construction and closes it on drop. Use it to bracket the per-frame
/// write sequence so supporting terminals composite the whole frame
/// atomically — no flicker between row writes. Terminals without
/// DEC 2026 ignore the escapes and behave as before.
///
/// Drop is best-effort — `end_sync` errors are swallowed, since `Drop`
/// can't propagate `io::Result`. In practice this only matters if the
/// underlying write fails, in which case the next frame's write will
/// surface the error.
pub struct SyncGuard<'a, B>
where
    B: Backend<Error = io::Error> + Write,
{
    terminal: &'a mut Terminal<InlineBackend<B>>,
}

impl<'a, B> SyncGuard<'a, B>
where
    B: Backend<Error = io::Error> + Write,
{
    pub fn new(terminal: &'a mut Terminal<InlineBackend<B>>) -> io::Result<Self> {
        terminal
            .backend_mut()
            .inner
            .queue(BeginSynchronizedUpdate)?;
        Ok(Self { terminal })
    }

    pub fn terminal(&mut self) -> &mut Terminal<InlineBackend<B>> {
        self.terminal
    }
}

impl<B> Drop for SyncGuard<'_, B>
where
    B: Backend<Error = io::Error> + Write,
{
    fn drop(&mut self) {
        let backend = self.terminal.backend_mut();
        let _ = backend.inner.queue(EndSynchronizedUpdate);
        let _ = Backend::flush(&mut backend.inner);
    }
}
