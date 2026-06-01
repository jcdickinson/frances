use serde::{Deserialize, Serialize};

/// Hashed identifier for the invoking process's controlling TTY, used as a
/// session-link filename.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TtyKey(pub String);

impl TtyKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TtyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for TtyKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
