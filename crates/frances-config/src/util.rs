//! Small helper types layered on top of the binding machinery.

use std::collections::BTreeSet;
use std::fmt;

use serde::de::{Deserialize, Deserializer, IgnoredAny, MapAccess, Visitor};

/// Deserialises any map-shaped config node and keeps only the keys.
///
/// Bound via [`ConfigHandle::bind::<Keys>(path)`](crate::ConfigHandle::bind)
/// to track which entries exist under a map (e.g. `models`,
/// `model_providers`) without paying to deserialise the values. Refreshes
/// flow through the normal binding pipeline, so subscribers see the new
/// key-set as entries appear and disappear.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keys(BTreeSet<String>);

impl Keys {
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.0.contains(key)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Compute the change from `previous` to `self`. `added` is keys in
    /// `self` that are absent from `previous`; `removed` is the reverse.
    pub fn diff(&self, previous: &Keys) -> KeysDiff {
        KeysDiff {
            added: self.0.difference(&previous.0).cloned().collect(),
            removed: previous.0.difference(&self.0).cloned().collect(),
        }
    }
}

/// The set of keys added and removed between two [`Keys`] snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeysDiff {
    pub added: BTreeSet<String>,
    pub removed: BTreeSet<String>,
}

impl KeysDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

impl<'de> Deserialize<'de> for Keys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeysVisitor;

        impl<'de> Visitor<'de> for KeysVisitor {
            type Value = Keys;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Keys, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut keys = BTreeSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    map.next_value::<IgnoredAny>()?;
                    keys.insert(key);
                }
                Ok(Keys(keys))
            }
        }

        deserializer.deserialize_map(KeysVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Configuration;
    use crate::event::ConfigEvent;
    use crate::value::{Path, Value};

    fn ev(path: &str, value: impl Into<Value>) -> ConfigEvent {
        ConfigEvent::new(Path::parse(path), value)
    }

    #[test]
    fn collects_map_keys_from_snapshot() {
        let cfg = Configuration::default()
            .applied(ev("models::alpha::id", "a"))
            .applied(ev("models::beta::id", "b"))
            .applied(ev("models::gamma::id", "g"));

        let binding = cfg.get("models").bind::<Keys>().unwrap();
        let keys = binding.get().expect("path resolves");

        assert_eq!(keys.len(), 3);
        assert!(keys.contains("alpha"));
        assert!(keys.contains("beta"));
        assert!(keys.contains("gamma"));

        let collected: Vec<&str> = keys.iter().collect();
        assert_eq!(collected, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn absent_path_yields_no_value() {
        let cfg = Configuration::default().applied(ev("other::x", "1"));
        let binding = cfg.get("models").bind::<Keys>().unwrap();
        assert!(binding.get().is_none());
    }
}
