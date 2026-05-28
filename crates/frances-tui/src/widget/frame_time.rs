//! [`FrameTime`] — animation clock the render pipeline hands to
//! widgets via [`RenderContext`]. A monotonic frame index expressed
//! in 60fps units (so 1.0 == one 60th of a second). The epoch is
//! arbitrary; only deltas matter for animation.
//!
//! The host wires up a [`WallClockFrameTime`] once at startup; widget
//! code never touches an [`Instant`] directly. Test fixtures pick
//! [`FixedFrameTime`] when they don't care about animation, or
//! [`AtomicFrameTime`] when they want to step the clock between
//! renders to assert per-frame behaviour.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub trait FrameTime: Send + Sync {
    /// Current frame index at 60fps. Monotonic; the epoch is arbitrary.
    fn get_frame(&self) -> f64;
}

/// Real clock — frame index = `start.elapsed().as_secs_f64() * 60.0`.
pub struct WallClockFrameTime {
    start: Instant,
}

impl WallClockFrameTime {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for WallClockFrameTime {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameTime for WallClockFrameTime {
    fn get_frame(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 60.0
    }
}

/// Constant frame — for widget tests that don't exercise animation.
pub struct FixedFrameTime(pub f64);

impl FrameTime for FixedFrameTime {
    fn get_frame(&self) -> f64 {
        self.0
    }
}

/// Frame backed by an atomic, so a test can advance the clock between
/// renders without holding a `&mut`. `f64` isn't atomic on its own;
/// we round-trip through `to_bits` / `from_bits`.
pub struct AtomicFrameTime {
    bits: AtomicU64,
}

impl AtomicFrameTime {
    pub fn new(frame: f64) -> Self {
        Self {
            bits: AtomicU64::new(frame.to_bits()),
        }
    }

    pub fn set(&self, frame: f64) {
        self.bits.store(frame.to_bits(), Ordering::Relaxed);
    }
}

impl FrameTime for AtomicFrameTime {
    fn get_frame(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }
}
