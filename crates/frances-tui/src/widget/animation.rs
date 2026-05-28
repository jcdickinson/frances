//! [`AnimationGate`] / [`AnimationLease`] — RAII handshake between
//! widgets that want to be repainted at a steady cadence and the
//! host's redraw loop.
//!
//! Each animated widget takes a lease (`ctx.animation_lease()`) while
//! it wants animation; the lease bumps a shared atomic counter on
//! creation and decrements on `Drop`. The host watches the counter
//! ([`AnimationGate::active`]) and ticks its wake-up timer only while
//! it's non-zero. No widget-specific knowledge ever crosses into the
//! run loop — the renderer doesn't need to know what's animating, only
//! that *something* is.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Default)]
pub struct AnimationGate {
    count: Arc<AtomicUsize>,
}

impl AnimationGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a lease. Increments the active count; drop the returned
    /// lease to release.
    pub fn lease(&self) -> AnimationLease {
        AnimationLease::new(Arc::clone(&self.count))
    }

    /// Outstanding lease count. The host's wake-up loop checks this
    /// to decide whether to keep ticking the animation timer.
    pub fn active(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }
}

pub struct AnimationLease {
    target: Arc<AtomicUsize>,
}

impl AnimationLease {
    fn new(target: Arc<AtomicUsize>) -> Self {
        target.fetch_add(1, Ordering::Relaxed);
        Self { target }
    }
}

impl Drop for AnimationLease {
    fn drop(&mut self) {
        self.target.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_bumps_and_drop_releases() {
        let gate = AnimationGate::new();
        assert_eq!(gate.active(), 0);
        let a = gate.lease();
        assert_eq!(gate.active(), 1);
        let b = gate.lease();
        assert_eq!(gate.active(), 2);
        drop(a);
        assert_eq!(gate.active(), 1);
        drop(b);
        assert_eq!(gate.active(), 0);
    }

    #[test]
    fn clones_share_the_same_counter() {
        let gate_a = AnimationGate::new();
        let gate_b = gate_a.clone();
        let _lease = gate_a.lease();
        assert_eq!(gate_b.active(), 1);
    }
}
