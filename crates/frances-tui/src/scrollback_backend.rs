//! [`ScrollbackBackend`] — a `ratatui::backend::Backend` wrapper that
//! serves two roles on the same struct, distinguished by `mode`:
//!
//! * **`Scrollback` mode (default).** Transparent passthrough; the
//!   `ScrollbackContainer` drives the terminal directly via row-level
//!   helpers — `ScrollbackBackend::move_cursor_abs`,
//!   `ScrollbackBackend::write_row`, `ScrollbackBackend::newline` —
//!   and `\n`s push old content into native scrollback.
//! * **`Footer` mode.** ratatui's `Terminal::draw` is in charge of the
//!   footer rect. [`Backend::size`] reports the footer's dimensions
//!   (not the whole band), and [`Backend::draw`] translates cell
//!   coordinates by `footer_anchor_y` before emitting, so a buffer
//!   ratatui owns at logical `(0, 0)` lands at the right screen row.
//!
//! [`SyncGuard`] brackets a frame with DEC mode 2026 synchronised
//! output so the cell stream composites atomically on supporting
//! terminals.

use std::io::{self, Write};

use crossterm::QueueableCommand;
use crossterm::cursor::MoveTo;
use crossterm::style::Print;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::Modifier;

pub struct ScrollbackBackend<B: Backend<Error = io::Error> + Write> {
    inner: B,
    terminal_size: Size,
    mode: BackendMode,
    /// Screen row where the footer rect starts. Only consulted in
    /// `Footer` mode (translates cell `y` for `Backend::draw`); ignored
    /// in `Scrollback` mode.
    footer_anchor_y: u16,
    /// Row count of the footer rect. Reported as `Backend::size().height`
    /// in `Footer` mode, ignored in `Scrollback` mode.
    footer_height: u16,
}

/// Which interface the backend is presenting this call. `Scrollback`
/// drives the band directly via row helpers; `Footer` lets ratatui's
/// `Terminal::draw` run its buffer-pair diff against the footer rect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BackendMode {
    Scrollback,
    Footer,
}

impl<B: Backend<Error = io::Error> + Write> ScrollbackBackend<B> {
    /// Construct a ScrollbackBackend over `inner`. The container's cursor
    /// origin is the caller's responsibility — pass it to
    /// `ScrollbackContainer::new`.
    pub fn new(inner: B, terminal_size: Size) -> Self {
        Self {
            inner,
            terminal_size,
            mode: BackendMode::Scrollback,
            footer_anchor_y: 0,
            footer_height: 0,
        }
    }

    pub fn terminal_size(&self) -> Size {
        self.terminal_size
    }

    pub(crate) fn set_mode(&mut self, mode: BackendMode) {
        if self.mode != mode {
            tracing::trace!(?mode, "backend set_mode");
        }
        self.mode = mode;
    }

    pub(crate) fn set_footer_rect(&mut self, anchor_y: u16, height: u16) {
        tracing::trace!(anchor_y, height, "backend set_footer_rect");
        self.footer_anchor_y = anchor_y;
        self.footer_height = height;
    }

    /// Crate-private peek at the underlying backend, used by tests
    /// to introspect a simulated terminal state after a draw.
    #[cfg(test)]
    pub(crate) fn inner(&self) -> &B {
        &self.inner
    }

    /// Move the cursor to an absolute screen position.
    pub(crate) fn move_cursor_abs(&mut self, x: u16, y: u16) -> io::Result<()> {
        tracing::trace!(x, y, "backend move_cursor_abs");
        self.inner.queue(MoveTo(x, y))?;
        Ok(())
    }

    /// Write one terminal row's worth of cells at the current cursor
    /// position, applying each cell's ANSI styling. Does **not** emit
    /// a trailing newline — the caller controls that via [`newline`].
    ///
    /// Wide-character handling: when a cell's symbol has display
    /// width > 1 (an emoji, CJK glyph, etc.), the terminal advances
    /// the cursor by that many columns when it prints the glyph.
    /// ratatui's `Buffer::set_string` represents this by storing the
    /// wide character in the primary cell and resetting the
    /// continuation cell(s) to `Cell::default()` (symbol `" "`).
    /// Naively printing every cell would emit an extra space for
    /// each continuation, shifting all subsequent cells right by one
    /// — visually corrupting the row and overflowing into the next.
    /// We skip those continuation cells.
    pub(crate) fn write_row<'a, I>(&mut self, cells: I) -> io::Result<()>
    where
        I: Iterator<Item = &'a Cell>,
    {
        use crossterm::style::Print;
        use unicode_width::UnicodeWidthStr;
        let mut skip_continuation = 0;
        for cell in cells {
            if skip_continuation > 0 {
                skip_continuation -= 1;
                continue;
            }
            queue_cell_style(&mut self.inner, cell)?;
            self.inner.queue(Print(cell.symbol()))?;
            skip_continuation = UnicodeWidthStr::width(cell.symbol()).saturating_sub(1);
        }
        reset_terminal_style(&mut self.inner)?;
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
        tracing::trace!(y, "backend clear_line");
        use crossterm::terminal::{Clear, ClearType as CtClearType};
        self.inner.queue(MoveTo(0, y))?;
        self.inner.queue(Clear(CtClearType::CurrentLine))?;
        Ok(())
    }

    /// React to a terminal-size change.
    pub fn handle_terminal_resize(&mut self, new_size: Size) -> io::Result<()> {
        tracing::trace!(
            w = new_size.width,
            h = new_size.height,
            "backend handle_terminal_resize",
        );
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
        tracing::trace!("backend clear_below_home");
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

fn queue_cell_style<W: Write>(writer: &mut W, cell: &Cell) -> io::Result<()> {
    reset_terminal_style(writer)?;
    writer.queue(crossterm::style::SetForegroundColor(to_crossterm_color(
        cell.fg,
    )))?;
    writer.queue(crossterm::style::SetBackgroundColor(to_crossterm_color(
        cell.bg,
    )))?;
    queue_modifier(writer, cell.modifier)
}

fn reset_terminal_style<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.queue(crossterm::style::ResetColor)?;
    writer.queue(crossterm::style::SetAttribute(
        crossterm::style::Attribute::Reset,
    ))?;
    Ok(())
}

fn queue_modifier<W: Write>(writer: &mut W, modifier: Modifier) -> io::Result<()> {
    use crossterm::style::{Attribute, SetAttribute};

    for (flag, attribute) in [
        (Modifier::BOLD, Attribute::Bold),
        (Modifier::DIM, Attribute::Dim),
        (Modifier::ITALIC, Attribute::Italic),
        (Modifier::UNDERLINED, Attribute::Underlined),
        (Modifier::SLOW_BLINK, Attribute::SlowBlink),
        (Modifier::RAPID_BLINK, Attribute::RapidBlink),
        (Modifier::REVERSED, Attribute::Reverse),
        (Modifier::HIDDEN, Attribute::Hidden),
        (Modifier::CROSSED_OUT, Attribute::CrossedOut),
    ] {
        if modifier.contains(flag) {
            writer.queue(SetAttribute(attribute))?;
        }
    }

    Ok(())
}

impl<B: Backend<Error = io::Error> + Write> Write for ScrollbackBackend<B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

// `ScrollbackContainer` uses ratatui's `Terminal::draw` only when the
// backend is in `Footer` mode — the rest of the time we drive cell
// emission directly via the row helpers above. In `Footer` mode `size`
// and `draw` are scoped to the footer rect so ratatui's buffer-pair
// diff runs against just that area.
impl<B: Backend<Error = io::Error> + Write> Backend for ScrollbackBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        // We emit cells via `queue!` directly rather than delegating to
        // `self.inner.draw`. Two reasons: (a) `Footer` mode needs to
        // offset cell `y` by `footer_anchor_y` before the bytes hit the
        // wire, (b) keeping a single emission path lets the test mock
        // (which carries a no-op `Backend::draw` but a real `Write`
        // impl that feeds an alacritty parser) see footer cells.
        use crossterm::style::Print;
        let dy = match self.mode {
            BackendMode::Scrollback => 0,
            BackendMode::Footer => self.footer_anchor_y,
        };
        let mut last_pos: Option<(u16, u16)> = None;
        for (x, y_local, cell) in content {
            let y = y_local + dy;
            let contiguous = matches!(last_pos, Some((px, py)) if py == y && px + 1 == x);
            if !contiguous {
                self.inner.queue(MoveTo(x, y))?;
            }
            last_pos = Some((x, y));
            queue_cell_style(&mut self.inner, cell)?;
            self.inner.queue(Print(cell.symbol()))?;
        }
        reset_terminal_style(&mut self.inner)?;
        Ok(())
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
        match (self.mode, clear_type) {
            // Scoped to the footer rect: ratatui calls this during
            // resize (which our container triggers when the footer
            // moves or changes height). Wiping the whole terminal
            // would take out the scrollback above it.
            (BackendMode::Footer, ClearType::All) => {
                tracing::trace!(
                    from_y = self.footer_anchor_y,
                    to_y_excl = self.footer_anchor_y + self.footer_height,
                    "backend clear_region(All) in Footer mode",
                );
                use crossterm::terminal::{Clear, ClearType as CtClearType};
                for y in self.footer_anchor_y..(self.footer_anchor_y + self.footer_height) {
                    self.inner.queue(MoveTo(0, y))?;
                    self.inner.queue(Clear(CtClearType::CurrentLine))?;
                }
                Ok(())
            }
            _ => {
                tracing::trace!(?clear_type, mode = ?self.mode, "backend clear_region passthrough");
                self.inner.clear_region(clear_type)
            }
        }
    }

    fn size(&self) -> Result<Size, Self::Error> {
        Ok(match self.mode {
            BackendMode::Scrollback => self.terminal_size,
            BackendMode::Footer => Size {
                width: self.terminal_size.width,
                height: self.footer_height,
            },
        })
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        let mut ws = self.inner.window_size()?;
        ws.columns_rows = match self.mode {
            BackendMode::Scrollback => self.terminal_size,
            BackendMode::Footer => Size {
                width: self.terminal_size.width,
                height: self.footer_height,
            },
        };
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
    terminal: &'a mut Terminal<ScrollbackBackend<B>>,
}

impl<'a, B> SyncGuard<'a, B>
where
    B: Backend<Error = io::Error> + Write,
{
    pub fn new(terminal: &'a mut Terminal<ScrollbackBackend<B>>) -> io::Result<Self> {
        terminal
            .backend_mut()
            .inner
            .queue(BeginSynchronizedUpdate)?;
        Ok(Self { terminal })
    }

    pub fn terminal(&mut self) -> &mut Terminal<ScrollbackBackend<B>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `Write` + `Backend` mock that records every byte written.
    struct RecorderBackend {
        buf: Vec<u8>,
        size: Size,
    }

    impl Write for RecorderBackend {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buf.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Backend for RecorderBackend {
        type Error = io::Error;
        fn draw<'a, I>(&mut self, _content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            Ok(())
        }
        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            Ok(Position { x: 0, y: 0 })
        }
        fn set_cursor_position<P: Into<Position>>(&mut self, _: P) -> Result<(), Self::Error> {
            Ok(())
        }
        fn clear(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
        fn clear_region(&mut self, _: ClearType) -> Result<(), Self::Error> {
            Ok(())
        }
        fn size(&self) -> Result<Size, Self::Error> {
            Ok(self.size)
        }
        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            Ok(WindowSize {
                columns_rows: self.size,
                pixels: Size {
                    width: 0,
                    height: 0,
                },
            })
        }
        fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn make() -> ScrollbackBackend<RecorderBackend> {
        ScrollbackBackend::new(
            RecorderBackend {
                buf: Vec::new(),
                size: Size {
                    width: 80,
                    height: 24,
                },
            },
            Size {
                width: 80,
                height: 24,
            },
        )
    }

    fn styled_cell(symbol: &str, modifier: Modifier) -> Cell {
        let mut cell = Cell::default();
        cell.set_symbol(symbol);
        cell.set_style(ratatui::style::Style::default().add_modifier(modifier));
        cell
    }

    fn ansi_output(backend: &ScrollbackBackend<RecorderBackend>) -> &str {
        std::str::from_utf8(&backend.inner().buf).unwrap()
    }

    #[test]
    fn write_row_preserves_cell_modifiers() {
        let mut backend = make();
        let cells = vec![
            styled_cell("B", Modifier::BOLD),
            styled_cell("I", Modifier::ITALIC),
            styled_cell("R", Modifier::REVERSED | Modifier::CROSSED_OUT),
        ];

        backend.write_row(cells.iter()).unwrap();

        let ansi = ansi_output(&backend);
        assert!(ansi.contains("\x1b[1mB"), "expected bold B, got {ansi:?}");
        assert!(ansi.contains("\x1b[3mI"), "expected italic I, got {ansi:?}");
        assert!(
            ansi.contains("\x1b[7m\x1b[9mR"),
            "expected reversed + crossed-out R, got {ansi:?}",
        );
        assert!(
            ansi.ends_with("\x1b[0m"),
            "expected trailing attribute reset, got {ansi:?}",
        );
    }

    #[test]
    fn draw_preserves_cell_modifiers() {
        let mut backend = make();
        backend.set_footer_rect(10, 2);
        backend.set_mode(BackendMode::Footer);
        let bold = styled_cell("B", Modifier::BOLD);
        let dim_reversed = styled_cell("R", Modifier::DIM | Modifier::REVERSED);
        let cells = vec![(0u16, 0u16, &bold), (1u16, 0u16, &dim_reversed)];

        backend.draw(cells.into_iter()).unwrap();

        let ansi = ansi_output(&backend);
        assert!(
            ansi.contains("\x1b[11;1H"),
            "expected footer-relative MoveTo before styled cells, got {ansi:?}",
        );
        assert!(ansi.contains("\x1b[1mB"), "expected bold B, got {ansi:?}");
        assert!(
            ansi.contains("\x1b[2m\x1b[7mR"),
            "expected dim + reversed R, got {ansi:?}",
        );
        assert!(
            ansi.ends_with("\x1b[0m"),
            "expected trailing attribute reset, got {ansi:?}",
        );
    }

    #[test]
    fn size_returns_band_in_scrollback_mode() {
        let backend = make();
        assert_eq!(
            backend.size().unwrap(),
            Size {
                width: 80,
                height: 24
            }
        );
    }

    #[test]
    fn size_returns_footer_dims_in_footer_mode() {
        let mut backend = make();
        backend.set_footer_rect(20, 3);
        backend.set_mode(BackendMode::Footer);
        assert_eq!(
            backend.size().unwrap(),
            Size {
                width: 80,
                height: 3
            }
        );
    }

    #[test]
    fn footer_mode_draw_offsets_cell_y_by_footer_anchor() {
        let mut backend = make();
        backend.set_footer_rect(10, 2);
        backend.set_mode(BackendMode::Footer);

        let mut cell = ratatui::buffer::Cell::default();
        cell.set_symbol("X");
        let cells = vec![(0u16, 0u16, &cell), (1u16, 0u16, &cell)];
        backend.draw(cells.into_iter()).unwrap();

        // First cell triggers MoveTo(0, 10) — i.e. `\x1b[11;1H` (1-based).
        // The second cell is contiguous, so no second MoveTo is emitted.
        let bytes = &backend.inner.buf;
        let ansi = std::str::from_utf8(bytes).unwrap();
        assert!(
            ansi.contains("\x1b[11;1H"),
            "expected MoveTo(0, 10) (= ESC [ 11 ; 1 H), got {ansi:?}",
        );
        assert!(ansi.contains("X"));
    }

    #[test]
    fn scrollback_mode_draw_does_not_offset_y() {
        let mut backend = make();
        backend.set_footer_rect(10, 2);
        // mode stays Scrollback (default).

        let mut cell = ratatui::buffer::Cell::default();
        cell.set_symbol("X");
        let cells = vec![(0u16, 5u16, &cell)];
        backend.draw(cells.into_iter()).unwrap();

        let ansi = std::str::from_utf8(&backend.inner.buf).unwrap();
        assert!(
            ansi.contains("\x1b[6;1H"),
            "expected MoveTo(0, 5) (= ESC [ 6 ; 1 H), got {ansi:?}",
        );
    }

    #[test]
    fn footer_mode_clear_region_all_blanks_only_the_footer_rect() {
        let mut backend = make();
        backend.set_footer_rect(20, 3);
        backend.set_mode(BackendMode::Footer);
        backend.clear_region(ClearType::All).unwrap();

        let ansi = std::str::from_utf8(&backend.inner.buf).unwrap();
        // Should issue MoveTo for rows 20, 21, 22 (1-based: 21, 22, 23).
        for row_1based in &["21", "22", "23"] {
            let expected = format!("\x1b[{row_1based};1H");
            assert!(
                ansi.contains(&expected),
                "expected MoveTo for row {row_1based}, got {ansi:?}",
            );
        }
        // Row 24 must NOT be touched — that's outside the footer rect.
        assert!(
            !ansi.contains("\x1b[24;1H"),
            "row 24 (outside footer) should not be cleared: {ansi:?}",
        );
    }
}
