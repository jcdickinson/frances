pub mod blocks;
pub mod footer;
pub mod status;

pub use blocks::{RawBlock, block_for_kind};
pub use footer::Footer;
#[expect(
    unused_imports,
    reason = "reused by the input-status renderer in a follow-up; remove the expect once that lands"
)]
pub use status::{StatusTone, status_prefix};
