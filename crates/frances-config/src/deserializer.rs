use std::any::type_name;
use std::sync::Arc;

use serde::Deserializer;
use serde::de::{DeserializeSeed, IntoDeserializer, MapAccess, SeqAccess, Visitor};

use crate::config::Configuration;
use crate::error::ConfigBindError;
use crate::value::{SEPARATOR, Value};

/// Custom deserializer over a [`Configuration`] subtree. Handles both
/// hierarchical structs/maps/seqs and per-leaf type coercion: when serde
/// asks for a numeric and the leaf is a [`Value::String`], we parse on the
/// fly.
pub(crate) struct ConfigDeserializer<'a> {
    path: Arc<str>,
    config: &'a Configuration,
}

impl<'a> ConfigDeserializer<'a> {
    pub(crate) fn new(path: Arc<str>, config: &'a Configuration) -> Self {
        Self { path, config }
    }

    fn wrap_err<T>(
        self,
        f: impl FnOnce(&Self) -> Result<T, ConfigBindError>,
    ) -> Result<T, ConfigBindError> {
        let path = self.path.clone();
        f(&self).map_err(|e| e.add_path(&path))
    }

    fn type_conversion<T>(value: &Value, target: &'static str) -> Result<T, ConfigBindError> {
        Err(ConfigBindError::TypeConversion {
            path: Arc::from(""),
            target,
            value: Arc::from(value.coerce_string().as_ref()),
        })
    }

    fn required<T>(target: &'static str) -> Result<T, ConfigBindError> {
        Err(ConfigBindError::RequiredValue {
            path: Arc::from(""),
            target_type: target,
        })
    }

    /// True iff every direct child key is `Value::Int(n)` for n in 0..len.
    fn is_seq(&self) -> bool {
        let children = &self.config.inner().children;
        if children.is_empty() {
            return false;
        }
        for i in 0..children.len() {
            let key = Value::Int(i as i64);
            if !children.contains_key(&key) {
                return false;
            }
        }
        true
    }
}

macro_rules! deserialize_int {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            self.wrap_err(|s| match s.config.value() {
                Some(Value::Int(i)) => <$ty>::try_from(*i)
                    .map_err(|_| ConfigBindError::TypeConversion {
                        path: Arc::from(""),
                        target: type_name::<$ty>(),
                        value: Arc::from(i.to_string()),
                    })
                    .and_then(|n| visitor.$visit(n)),
                Some(v) => v
                    .coerce_string()
                    .parse::<$ty>()
                    .map_err(|_| ConfigBindError::TypeConversion {
                        path: Arc::from(""),
                        target: type_name::<$ty>(),
                        value: Arc::from(v.coerce_string().as_ref()),
                    })
                    .and_then(|n| visitor.$visit(n)),
                None => Self::required(type_name::<$ty>()),
            })
        }
    };
}

macro_rules! deserialize_float {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            self.wrap_err(|s| match s.config.value() {
                Some(Value::Float(f)) => visitor.$visit(*f as $ty),
                Some(Value::Int(i)) => visitor.$visit(*i as $ty),
                Some(v) => v
                    .coerce_string()
                    .parse::<$ty>()
                    .map_err(|_| ConfigBindError::TypeConversion {
                        path: Arc::from(""),
                        target: type_name::<$ty>(),
                        value: Arc::from(v.coerce_string().as_ref()),
                    })
                    .and_then(|n| visitor.$visit(n)),
                None => Self::required(type_name::<$ty>()),
            })
        }
    };
}

impl<'de, 'a> Deserializer<'de> for ConfigDeserializer<'a> {
    type Error = ConfigBindError;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let path = self.path.clone();
        let result = if self.config.value().is_some() {
            self.deserialize_str(visitor)
        } else if self.is_seq() {
            self.deserialize_seq(visitor)
        } else {
            self.deserialize_map(visitor)
        };
        result.map_err(|e| e.add_path(&path))
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.wrap_err(|s| match s.config.value() {
            Some(Value::Bool(b)) => visitor.visit_bool(*b),
            Some(v) => match v.coerce_string().as_ref() {
                "true" | "True" | "TRUE" | "1" => visitor.visit_bool(true),
                "false" | "False" | "FALSE" | "0" => visitor.visit_bool(false),
                _ => Self::type_conversion(v, type_name::<bool>()),
            },
            None => Self::required(type_name::<bool>()),
        })
    }

    deserialize_int!(deserialize_i8, visit_i8, i8);
    deserialize_int!(deserialize_i16, visit_i16, i16);
    deserialize_int!(deserialize_i32, visit_i32, i32);
    deserialize_int!(deserialize_i64, visit_i64, i64);
    deserialize_int!(deserialize_u8, visit_u8, u8);
    deserialize_int!(deserialize_u16, visit_u16, u16);
    deserialize_int!(deserialize_u32, visit_u32, u32);
    deserialize_int!(deserialize_u64, visit_u64, u64);
    deserialize_float!(deserialize_f32, visit_f32, f32);
    deserialize_float!(deserialize_f64, visit_f64, f64);

    fn deserialize_char<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.wrap_err(|s| match s.config.value() {
            Some(v) => {
                let s = v.coerce_string();
                let mut chars = s.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => visitor.visit_char(c),
                    _ => Self::type_conversion(v, type_name::<char>()),
                }
            }
            None => Self::required(type_name::<char>()),
        })
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.wrap_err(|s| match s.config.value() {
            Some(v) => visitor.visit_str(&v.coerce_string()),
            None => Self::required(type_name::<&str>()),
        })
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.wrap_err(|s| match s.config.value() {
            Some(v) => visitor.visit_bytes(v.coerce_string().as_bytes()),
            None => Self::required(type_name::<&[u8]>()),
        })
    }

    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let path = self.path.clone();
        let has_value = self.config.value().is_some();
        let has_children = !self.config.inner().children.is_empty();
        if has_value || has_children {
            visitor.visit_some(self).map_err(|e| e.add_path(&path))
        } else {
            visitor.visit_none()
        }
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let path = self.path.clone();
        visitor
            .visit_newtype_struct(self)
            .map_err(|e: ConfigBindError| e.add_path(&path))
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let path = self.path.clone();
        visitor
            .visit_seq(ConfigSeqAccess::new(self.path.clone(), self.config))
            .map_err(|e: ConfigBindError| e.add_path(&path))
    }

    fn deserialize_tuple<V>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        let path = self.path.clone();
        visitor
            .visit_map(ConfigMapAccess::new(self.path.clone(), self.config))
            .map_err(|e: ConfigBindError| e.add_path(&path))
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.wrap_err(|s| match s.config.value() {
            Some(v) => {
                let owned = v.coerce_string().into_owned();
                visitor.visit_enum(owned.into_deserializer())
            }
            None => Self::required(type_name::<V>()),
        })
    }

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }
}

struct ConfigSeqAccess<'a> {
    parent_path: Arc<str>,
    config: &'a Configuration,
    index: usize,
}

impl<'a> ConfigSeqAccess<'a> {
    fn new(parent_path: Arc<str>, config: &'a Configuration) -> Self {
        Self {
            parent_path,
            config,
            index: 0,
        }
    }
}

impl<'de, 'a> SeqAccess<'de> for ConfigSeqAccess<'a> {
    type Error = ConfigBindError;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: DeserializeSeed<'de>,
    {
        let key = Value::Int(self.index as i64);
        match self.config.inner().children.get(&key) {
            Some(child) => {
                let child_path = join_path(&self.parent_path, &self.index.to_string());
                self.index += 1;
                seed.deserialize(ConfigDeserializer::new(child_path, child))
                    .map(Some)
            }
            None => Ok(None),
        }
    }
}

struct ConfigMapAccess<'a> {
    parent_path: Arc<str>,
    config: &'a Configuration,
    keys: std::vec::IntoIter<Value>,
    current_key: Option<Value>,
}

impl<'a> ConfigMapAccess<'a> {
    fn new(parent_path: Arc<str>, config: &'a Configuration) -> Self {
        let keys: Vec<Value> = config.inner().children.keys().cloned().collect();
        Self {
            parent_path,
            config,
            keys: keys.into_iter(),
            current_key: None,
        }
    }
}

impl<'de, 'a> MapAccess<'de> for ConfigMapAccess<'a> {
    type Error = ConfigBindError;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: DeserializeSeed<'de>,
    {
        match self.keys.next() {
            Some(key) => {
                let rendered = key.coerce_string().into_owned();
                self.current_key = Some(key);
                seed.deserialize(rendered.into_deserializer()).map(Some)
            }
            None => Ok(None),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: DeserializeSeed<'de>,
    {
        let key = self
            .current_key
            .take()
            .ok_or_else(|| ConfigBindError::Structural {
                path: self.parent_path.clone(),
                target_type: type_name::<V>(),
                error: Arc::from("next_value_seed called without key"),
            })?;
        let child =
            self.config
                .inner()
                .children
                .get(&key)
                .ok_or_else(|| ConfigBindError::Structural {
                    path: self.parent_path.clone(),
                    target_type: type_name::<V>(),
                    error: Arc::from(format!("key '{}' missing during map iteration", key)),
                })?;
        let segment = key.coerce_string();
        let child_path = join_path(&self.parent_path, segment.as_ref());
        seed.deserialize(ConfigDeserializer::new(child_path, child))
    }
}

fn join_path(parent: &str, segment: &str) -> Arc<str> {
    if parent.is_empty() {
        Arc::from(segment)
    } else {
        Arc::from(format!("{parent}{SEPARATOR}{segment}"))
    }
}
