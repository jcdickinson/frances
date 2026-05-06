use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;

use crate::anchor::Anchor;
use crate::store::AnchorStore;

pub struct Pool {
    used: HashSet<Anchor>,
    next: Anchor,
}

impl Pool {
    pub async fn load<S: AnchorStore + ?Sized>(store: &S, path: &Path) -> Result<Self> {
        let used = store.used_anchors(path).await?;
        Ok(Self::from_used(used))
    }

    pub fn from_used(used: HashSet<Anchor>) -> Self {
        Self {
            used,
            next: Anchor::first(),
        }
    }

    pub fn mint(&mut self) -> Anchor {
        loop {
            let candidate = self.next.clone();
            self.next.increment();
            if !self.used.contains(&candidate) {
                self.used.insert(candidate.clone());
                return candidate;
            }
        }
    }

    pub fn used_count(&self) -> usize {
        self.used.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_used_starts_fresh() {
        let pool = Pool::from_used(HashSet::new());
        assert_eq!(pool.used_count(), 0);
        assert_eq!(pool.next, Anchor::first());
    }

    #[test]
    fn mint_returns_canonical_order() {
        let mut pool = Pool::from_used(HashSet::new());
        let a = pool.mint();
        let b = pool.mint();
        let c = pool.mint();
        assert_eq!(a, Anchor::first());
        let mut expected = Anchor::first();
        expected.increment();
        assert_eq!(b, expected);
        expected.increment();
        assert_eq!(c, expected);
    }

    #[test]
    fn mint_skips_used() {
        let mut used = HashSet::new();
        used.insert(Anchor::first());
        let mut a2 = Anchor::first();
        a2.increment();
        a2.increment();
        used.insert(a2.clone());
        let mut pool = Pool::from_used(used);

        let mut expected_first = Anchor::first();
        expected_first.increment();
        assert_eq!(pool.mint(), expected_first);

        let mut expected_second = a2.clone();
        expected_second.increment();
        assert_eq!(pool.mint(), expected_second);
    }

    #[test]
    fn mint_records_in_used() {
        let mut pool = Pool::from_used(HashSet::new());
        let a = pool.mint();
        assert_eq!(pool.used_count(), 1);
        assert!(pool.mint() != a);
    }
}
