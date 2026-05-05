use std::io;
use std::io::Stdout;

use super::region::Region;

pub struct RenderCtx<'a> {
    pub stdout: &'a mut Stdout,
    pub viewport_top: u16,
    pub viewport_width: u16,
    pub viewport_height: u16,
}

pub trait Widget {
    fn measure(&self, max_width: u16) -> u16;
    fn render(&self, region: Region, ctx: &mut RenderCtx) -> io::Result<()>;
}
