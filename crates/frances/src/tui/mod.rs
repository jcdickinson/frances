pub mod block_view;
pub mod region;
pub mod screen;
pub mod scrollback;
pub mod textarea;
pub mod widget;

pub use block_view::BlockView;
#[expect(
    unused_imports,
    reason = "TUI module surface area; consumers wire incrementally"
)]
pub use region::Region;
pub use screen::Screen;
pub use textarea::Textarea;
#[expect(
    unused_imports,
    reason = "TUI module surface area; consumers wire incrementally"
)]
pub use widget::{RenderCtx, Widget};
