use std::io;

use crate::daemon::protocol::BlockKind;

use super::block_view::BlockView;
use super::screen::Screen;

pub fn commit_block(screen: &mut Screen, kind: BlockKind, text: &str) -> io::Result<()> {
    let lines = BlockView::new(kind, text).wrapped_lines(screen.width());
    screen.emit_above(&lines)
}

pub fn emit_text(screen: &mut Screen, lines: &[String]) -> io::Result<()> {
    screen.emit_above(lines)
}
