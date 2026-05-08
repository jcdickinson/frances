use std::io;
use std::io::Stdout;

use super::region::Region;

pub struct RenderCtx<'a> {
    pub stdout: &'a mut Stdout,
    pub viewport_top: u16,
    pub viewport_width: u16,
    #[expect(
        dead_code,
        reason = "render context plumbing; widgets that clip to the viewport will need this"
    )]
    pub viewport_height: u16,
}

pub trait Widget {
    #[expect(
        dead_code,
        reason = "trait scaffolding for the widget tree; layout pass not yet wired"
    )]
    fn measure(&self, max_width: u16) -> u16;
    fn render(&self, region: Region, ctx: &mut RenderCtx) -> io::Result<()>;
}
