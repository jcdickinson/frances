//! Wall-clock helpers shared across the workspace.

use std::time::{SystemTime, UNIX_EPOCH};

/// Nanoseconds since the Unix epoch, saturating to 0 if the clock is before the
/// epoch. Integrity timestamp, not a monotonic clock.
pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Whole seconds since the Unix epoch.
pub fn now_unix_secs() -> u64 {
    (now_ns() / 1_000_000_000).max(0) as u64
}
