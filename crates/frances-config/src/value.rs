use std::borrow::Cow;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::Arc;

use smallvec::SmallVec;

/// Path separator between segments in stringly-typed paths.
pub const SEPARATOR: &str = "::";

/// A typed config value. Both stored values and path components use this.
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// ASCII case-insensitive in `Eq`/`Hash`. The original casing is kept
    /// for display.
    String(Arc<str>),
}

impl Value {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Try to interpret this value as a non-negative array index. `Int`
    /// converts when non-negative; `String` parses when it looks like a
    /// non-negative integer.
    pub fn as_usize(&self) -> Option<usize> {
        match self {
            Value::Int(i) if *i >= 0 => usize::try_from(*i).ok(),
            Value::String(s) => s.parse::<usize>().ok(),
            _ => None,
        }
    }

    /// Render this value as a string regardless of variant. Used by the
    /// deserializer when serde asks for `&str`/`String`.
    pub fn coerce_string(&self) -> Cow<'_, str> {
        match self {
            Value::Null => Cow::Borrowed(""),
            Value::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
            Value::Int(i) => Cow::Owned(i.to_string()),
            Value::Float(f) => Cow::Owned(f.to_string()),
            Value::String(s) => Cow::Borrowed(s),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::String(a), Value::String(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        }
    }
}

impl Eq for Value {}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Value::Null => {}
            Value::Bool(b) => b.hash(state),
            Value::Int(i) => i.hash(state),
            Value::Float(f) => f.to_bits().hash(state),
            Value::String(s) => {
                for byte in s.bytes() {
                    byte.to_ascii_lowercase().hash(state);
                }
            }
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str(""),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::String(s) => f.write_str(s),
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(Arc::from(s))
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(Arc::from(s))
    }
}

impl From<Arc<str>> for Value {
    fn from(s: Arc<str>) -> Self {
        Value::String(s)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<i32> for Value {
    fn from(i: i32) -> Self {
        Value::Int(i.into())
    }
}

impl From<usize> for Value {
    fn from(i: usize) -> Self {
        Value::Int(i64::try_from(i).unwrap_or(i64::MAX))
    }
}

impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}

/// A structured path through the config tree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Path(SmallVec<[Value; 4]>);

impl Path {
    pub fn new() -> Self {
        Self(SmallVec::new())
    }

    pub fn push(&mut self, segment: impl Into<Value>) {
        self.0.push(segment.into());
    }

    pub fn iter(&self) -> impl Iterator<Item = &Value> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn segments(&self) -> &[Value] {
        &self.0
    }

    /// Append `other`'s segments to a clone of `self`.
    pub fn join(&self, other: impl Into<Path>) -> Path {
        let mut out = self.clone();
        out.0.extend(other.into().0);
        out
    }

    /// Parse from a separator-joined string. Numeric segments become
    /// [`Value::Int`]; everything else becomes [`Value::String`]. An empty
    /// input produces an empty path.
    pub fn parse(s: impl AsRef<str>) -> Self {
        let s = s.as_ref();
        if s.is_empty() {
            return Self::new();
        }
        let segments: SmallVec<[Value; 4]> = s.split(SEPARATOR).map(parse_segment).collect();
        Self(segments)
    }
}

fn parse_segment(s: &str) -> Value {
    if let Ok(i) = s.parse::<i64>() {
        Value::Int(i)
    } else {
        Value::String(Arc::from(s))
    }
}

impl FromStr for Path {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse(s))
    }
}

impl From<&str> for Path {
    fn from(s: &str) -> Self {
        Self::parse(s)
    }
}

impl From<&String> for Path {
    fn from(s: &String) -> Self {
        Self::parse(s.as_str())
    }
}

impl From<String> for Path {
    fn from(s: String) -> Self {
        Self::parse(s.as_str())
    }
}

impl From<Vec<Value>> for Path {
    fn from(v: Vec<Value>) -> Self {
        Self(v.into())
    }
}

impl<const N: usize> From<[Value; N]> for Path {
    fn from(arr: [Value; N]) -> Self {
        Self(SmallVec::from_iter(arr))
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for segment in &self.0 {
            if !first {
                f.write_str(SEPARATOR)?;
            }
            first = false;
            write!(f, "{segment}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_numeric_segment_becomes_int() {
        let p = Path::parse("a::0::b");
        assert_eq!(
            p.segments(),
            &[
                Value::String(Arc::from("a")),
                Value::Int(0),
                Value::String(Arc::from("b")),
            ]
        );
    }

    #[test]
    fn empty_path_parses_empty() {
        assert!(Path::parse("").is_empty());
    }

    #[test]
    fn display_roundtrips() {
        let p = Path::parse("foo::bar::42");
        assert_eq!(p.to_string(), "foo::bar::42");
    }

    #[test]
    fn string_eq_is_case_insensitive() {
        assert_eq!(
            Value::String(Arc::from("Foo")),
            Value::String(Arc::from("foo"))
        );
    }

    #[test]
    fn int_string_distinct() {
        assert_ne!(Value::Int(0), Value::String(Arc::from("0")));
    }

    #[test]
    fn as_usize_from_int_and_string() {
        assert_eq!(Value::Int(7).as_usize(), Some(7));
        assert_eq!(Value::String(Arc::from("7")).as_usize(), Some(7));
        assert_eq!(Value::Int(-1).as_usize(), None);
        assert_eq!(Value::String(Arc::from("nope")).as_usize(), None);
    }

    #[test]
    fn coerce_string_covers_all_variants() {
        assert_eq!(Value::Null.coerce_string(), "");
        assert_eq!(Value::Bool(true).coerce_string(), "true");
        assert_eq!(Value::Int(42).coerce_string(), "42");
        assert_eq!(Value::String(Arc::from("hi")).coerce_string(), "hi");
    }
}
