use std::io;

use super::screen::Screen;

pub fn emit_text(screen: &mut Screen, lines: &[String]) -> io::Result<()> {
    screen.emit_above(lines)
}
