use std::collections::HashMap;
use std::sync::{Arc, Weak};

use serde::de::DeserializeOwned;

use crate::binding::ConfigBinding;
use crate::error::ConfigBindError;
use crate::event::ConfigEvent;
use crate::value::{Path, Value};

/// An immutable snapshot of merged configuration.
///
/// Each [`ConfigEvent`] applied to a `Configuration` produces a new
/// `Configuration` with structural sharing for unaffected branches; cloning
/// is `Arc`-cheap.
#[derive(Debug, Clone, Default)]
pub struct Configuration {
    inner: Arc<ConfigInner>,
}

#[derive(Debug, Default)]
pub(crate) struct ConfigInner {
    pub value: Option<Value>,
    pub children: HashMap<Value, Configuration>,
}

impl Configuration {
    pub fn get(&self, path: impl Into<Path>) -> ConfigurationRef<'_> {
        let path = path.into();
        let mut cursor: Option<&Configuration> = Some(self);
        let mut traversed = Path::new();
        for segment in path.iter() {
            traversed.push(segment.clone());
            cursor = cursor
                .and_then(|c| c.inner.children.get(segment))
                .map(|c: &Configuration| c);
        }
        ConfigurationRef {
            path: traversed,
            config: cursor,
        }
    }

    pub fn value(&self) -> Option<&Value> {
        self.inner.value.as_ref()
    }

    pub fn bind<T>(&self) -> Result<ConfigBinding<T, T>, ConfigBindError>
    where
        T: DeserializeOwned + Send + Sync + 'static,
    {
        ConfigBinding::from_snapshot(Path::new(), Some(self), Weak::new())
    }

    pub(crate) fn inner(&self) -> &ConfigInner {
        &self.inner
    }

    /// Produce a new snapshot with `event` applied.
    ///
    /// `Value::Null` removes the leaf at `event.path`. Any other value is
    /// stored at the leaf, creating intermediate nodes as needed.
    /// Structural sharing means unaffected branches are not cloned.
    pub fn applied(&self, event: ConfigEvent) -> Self {
        let segments = event.path.segments();
        let new_inner = if event.value.is_null() {
            apply_unset(&self.inner, segments)
        } else {
            apply_set(&self.inner, segments, event.value)
        };
        match new_inner {
            Some(inner) => Self {
                inner: Arc::new(inner),
            },
            None => Self::default(),
        }
    }
}

/// Set the leaf at `segments` (relative to `node`) to `value`. Returns the
/// new node, or `None` if the result would be empty (which only happens at
/// the top level when applying to an empty tree with no segments — covered
/// by the wrapper).
fn apply_set(node: &ConfigInner, segments: &[Value], value: Value) -> Option<ConfigInner> {
    if segments.is_empty() {
        return Some(ConfigInner {
            value: Some(value),
            children: node.children.clone(),
        });
    }
    let (head, tail) = segments.split_first().expect("non-empty checked above");
    let empty;
    let existing = match node.children.get(head) {
        Some(c) => c.inner.as_ref(),
        None => {
            empty = ConfigInner::default();
            &empty
        }
    };
    let updated = apply_set(existing, tail, value)?;
    let mut children = node.children.clone();
    children.insert(
        head.clone(),
        Configuration {
            inner: Arc::new(updated),
        },
    );
    Some(ConfigInner {
        value: node.value.clone(),
        children,
    })
}

/// Remove the leaf at `segments`. Returns `None` if removing the leaf left an
/// empty subtree at this level (caller can then prune the parent's entry).
fn apply_unset(node: &ConfigInner, segments: &[Value]) -> Option<ConfigInner> {
    if segments.is_empty() {
        if node.children.is_empty() {
            return None;
        }
        return Some(ConfigInner {
            value: None,
            children: node.children.clone(),
        });
    }
    let (head, tail) = segments.split_first().expect("non-empty checked above");
    let Some(child) = node.children.get(head) else {
        return Some(ConfigInner {
            value: node.value.clone(),
            children: node.children.clone(),
        });
    };
    let mut children = node.children.clone();
    match apply_unset(child.inner.as_ref(), tail) {
        Some(updated) => {
            children.insert(
                head.clone(),
                Configuration {
                    inner: Arc::new(updated),
                },
            );
        }
        None => {
            children.remove(head);
        }
    }
    if node.value.is_none() && children.is_empty() {
        return None;
    }
    Some(ConfigInner {
        value: node.value.clone(),
        children,
    })
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
        let next = self.config.and_then(|c| c.inner.children.get(&segment));
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
}
