use std::collections::HashMap;
use std::sync::{Arc, Weak};

use serde::de::DeserializeOwned;

use crate::binding::ConfigBinding;
use crate::error::ConfigBindError;
use crate::event::{ConfigEvent, ProviderId};
use crate::value::{Path, Value};

/// An immutable snapshot of layered configuration.
///
/// Each node carries one slot per provider (indexed by [`ProviderId`]) plus
/// a cache of the highest-priority non-null slot, so reads are O(1) and
/// retraction by a higher-priority provider naturally falls through to the
/// next provider that has a value at the same path.
///
/// Cloning is cheap — three [`Arc`]s.
#[derive(Debug, Clone)]
pub struct Configuration {
    values: Arc<[Value]>,
    value_index: Option<usize>,
    children: Arc<HashMap<Value, Configuration>>,
}

impl Default for Configuration {
    fn default() -> Self {
        Self::empty(1)
    }
}

impl Configuration {
    /// Construct an empty configuration sized for `num_providers` layers.
    /// Every slot starts as [`Value::Null`].
    pub fn empty(num_providers: usize) -> Self {
        let values: Arc<[Value]> = (0..num_providers).map(|_| Value::Null).collect();
        Self {
            values,
            value_index: None,
            children: Arc::new(HashMap::new()),
        }
    }

    /// The highest-priority non-null slot at this node, or `None` if no
    /// provider has set a value here.
    pub fn value(&self) -> Option<&Value> {
        self.value_index.map(|i| &self.values[i])
    }

    pub(crate) fn children(&self) -> &HashMap<Value, Configuration> {
        &self.children
    }

    pub fn get(&self, path: impl Into<Path>) -> ConfigurationRef<'_> {
        let path = path.into();
        let mut cursor: Option<&Configuration> = Some(self);
        let mut traversed = Path::new();
        for segment in path.iter() {
            traversed.push(segment.clone());
            cursor = cursor.and_then(|c| c.children.get(segment));
        }
        ConfigurationRef {
            path: traversed,
            config: cursor,
        }
    }

    pub fn bind<T>(&self) -> Result<ConfigBinding<T, T>, ConfigBindError>
    where
        T: DeserializeOwned + Send + Sync + 'static,
    {
        ConfigBinding::from_snapshot(Path::new(), Some(self), Weak::new())
    }

    /// Ergonomic single-event update against [`ProviderId`] 0. Used by tests
    /// and ad-hoc snapshot construction. The processor uses the batch form.
    pub fn applied(&self, event: ConfigEvent) -> Self {
        self.applied_batch(ProviderId(0), std::slice::from_ref(&event))
            .unwrap_or_else(|| Self::empty(self.values.len()))
    }

    /// Apply a batch of events to a single provider's layer.
    ///
    /// Returns `None` when the entire subtree empties out — the caller (or
    /// the parent in the recursion) should drop this node. Returns
    /// `Some(updated)` otherwise. Sibling subtrees stay [`Arc`]-shared with
    /// the original.
    pub fn applied_batch(&self, provider: ProviderId, events: &[ConfigEvent]) -> Option<Self> {
        let entries: Vec<(&[Value], &Value)> = events
            .iter()
            .map(|e| (e.path.segments(), &e.value))
            .collect();
        apply_at_node(self, provider, &entries)
    }
}

/// Apply a batch of (remaining-path, value) entries to `node`'s subtree for
/// `provider`. Recurses by grouping entries on their head segment.
fn apply_at_node(
    node: &Configuration,
    provider: ProviderId,
    entries: &[(&[Value], &Value)],
) -> Option<Configuration> {
    let mut here: Option<&Value> = None;
    let mut by_child: HashMap<Value, Vec<(&[Value], &Value)>> = HashMap::new();
    for (path, value) in entries {
        match path.split_first() {
            Some((head, tail)) => {
                by_child
                    .entry(head.clone())
                    .or_default()
                    .push((tail, value));
            }
            None => {
                here = Some(value);
            }
        }
    }

    let (new_values, new_value_index) = match here {
        Some(v) => update_slot(&node.values, provider.index(), v),
        None => (node.values.clone(), node.value_index),
    };

    let mut new_children: HashMap<Value, Configuration> = (*node.children).clone();
    let mut children_changed = false;
    for (key, sub_entries) in by_child {
        let computed = match new_children.get(&key) {
            Some(child) => apply_at_node(child, provider, &sub_entries),
            None => {
                let empty = Configuration::empty(node.values.len());
                apply_at_node(&empty, provider, &sub_entries)
            }
        };
        match computed {
            Some(updated) => {
                new_children.insert(key, updated);
                children_changed = true;
            }
            None => {
                if new_children.remove(&key).is_some() {
                    children_changed = true;
                }
            }
        }
    }

    let here_changed = here.is_some();
    if !here_changed && !children_changed {
        return Some(node.clone());
    }

    if new_value_index.is_none() && new_children.is_empty() {
        return None;
    }

    Some(Configuration {
        values: new_values,
        value_index: new_value_index,
        children: Arc::new(new_children),
    })
}

/// Replace `values[slot]` with `new_value`, returning the new slice and the
/// recomputed highest-priority non-null index.
fn update_slot(
    values: &Arc<[Value]>,
    slot: usize,
    new_value: &Value,
) -> (Arc<[Value]>, Option<usize>) {
    let mut next: Vec<Value> = values.iter().cloned().collect();
    next[slot] = new_value.clone();
    let index = next.iter().rposition(|v| !v.is_null());
    (next.into(), index)
}

/// A borrowed view into a [`Configuration`] subtree, tracking the path so
/// errors can report meaningful locations.
#[derive(Debug, Clone)]
pub struct ConfigurationRef<'a> {
    pub(crate) path: Path,
    pub(crate) config: Option<&'a Configuration>,
}

impl<'a> ConfigurationRef<'a> {
    pub fn value(&self) -> Option<&'a Value> {
        self.config.and_then(|c| c.value())
    }

    pub fn get(&self, segment: impl Into<Value>) -> ConfigurationRef<'a> {
        let segment = segment.into();
        let mut path = self.path.clone();
        path.push(segment.clone());
        let next = self.config.and_then(|c| c.children.get(&segment));
        ConfigurationRef { path, config: next }
    }

    pub fn bind<T>(self) -> Result<ConfigBinding<T, T>, ConfigBindError>
    where
        T: DeserializeOwned + Send + Sync + 'static,
    {
        ConfigBinding::from_snapshot(self.path, self.config, Weak::new())
    }

    pub(crate) fn config(&self) -> Option<&'a Configuration> {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(path: &str, value: impl Into<Value>) -> ConfigEvent {
        ConfigEvent::new(Path::parse(path), value)
    }

    #[test]
    fn apply_then_get_returns_value() {
        let cfg = Configuration::default()
            .applied(ev("llm::model", "qwen"))
            .applied(ev("llm::tokens", 1000_i64));
        assert_eq!(
            cfg.get("llm::model").value(),
            Some(&Value::String("qwen".into()))
        );
        assert_eq!(cfg.get("llm::tokens").value(), Some(&Value::Int(1000)));
    }

    #[test]
    fn apply_null_removes_leaf() {
        let cfg = Configuration::default().applied(ev("a::b", "x"));
        assert!(cfg.get("a::b").value().is_some());
        let cfg = cfg.applied(ev("a::b", Value::Null));
        assert!(cfg.get("a::b").value().is_none());
        assert!(cfg.get("a").config().is_none());
    }

    #[test]
    fn case_insensitive_path_lookup() {
        let cfg = Configuration::default().applied(ev("Foo::Bar", "baz"));
        assert_eq!(
            cfg.get("foo::bar").value(),
            Some(&Value::String("baz".into()))
        );
    }

    #[test]
    fn array_indices_are_int_keyed() {
        let cfg = Configuration::default()
            .applied(ev("tags::0", "alpha"))
            .applied(ev("tags::1", "beta"));
        assert_eq!(
            cfg.get(Path::from(vec![
                Value::String("tags".into()),
                Value::Int(0)
            ]))
            .value(),
            Some(&Value::String("alpha".into()))
        );
    }

    #[test]
    fn higher_layer_overrides_lower() {
        let cfg = Configuration::empty(2)
            .applied_batch(ProviderId(0), &[ev("a::b", "low")])
            .unwrap()
            .applied_batch(ProviderId(1), &[ev("a::b", "high")])
            .unwrap();
        assert_eq!(cfg.get("a::b").value(), Some(&Value::String("high".into())));
    }

    #[test]
    fn unset_falls_through_to_lower_layer() {
        let cfg = Configuration::empty(2)
            .applied_batch(ProviderId(0), &[ev("a::b", "low")])
            .unwrap()
            .applied_batch(ProviderId(1), &[ev("a::b", "high")])
            .unwrap()
            .applied_batch(ProviderId(1), &[ev("a::b", Value::Null)])
            .unwrap();
        assert_eq!(cfg.get("a::b").value(), Some(&Value::String("low".into())));
    }
}
