//! Shell-style env templates with deferred resolution.
//!
//! [`EnvString`] holds a raw template string like `${HOME}/cache` or
//! `Bearer ${OPENROUTER_API_KEY}`. Expansion is performed at use time via
//! [`EnvString::expand`], which delegates to `shellexpand::env_with_context`.
//!
//! Semantics — `set -u`, with default forms still honoured:
//! - `$NAME` / `${NAME}` — substitute. Missing → error.
//! - `${NAME:-default}` / `${NAME-default}` — substitute, fall back to
//!   default if missing. Default is preferred over the missing-var error,
//!   so this form is always safe.
//! - To embed a literal `$`, double it: `$$`. shellexpand does **not**
//!   treat `\$` as an escape — backslashes pass through verbatim.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;

use serde::Deserialize;
use thiserror::Error;

/// A shell-style template (`$VAR`, `${VAR}`, `${VAR:-default}`).
///
/// Stored unparsed; expansion happens through `shellexpand` on each call
/// to [`expand`](Self::expand). For typical config values (a handful of
/// HTTP headers per request) the parse cost is negligible and saves us
/// from carrying an AST that mirrors shellexpand's internals.
#[derive(Debug, Clone)]
pub struct EnvString {
    raw: Arc<str>,
}

impl EnvString {
    pub fn new(s: impl Into<Arc<str>>) -> Self {
        Self { raw: s.into() }
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Resolve the template against `env`. Missing variables (with no
    /// default form) produce [`EnvStringExpandError::MissingVar`].
    pub fn expand(&self, env: &dyn EnvLookup) -> Result<String, EnvStringExpandError> {
        let lookup = |name: &str| -> Result<Option<&str>, MissingVar> {
            match env.get(name) {
                Some(value) => Ok(Some(value)),
                None => Err(MissingVar),
            }
        };
        shellexpand::env_with_context(&self.raw, lookup)
            .map(Cow::into_owned)
            .map_err(|err| EnvStringExpandError::MissingVar {
                template: Arc::clone(&self.raw),
                var: err.var_name,
            })
    }
}

impl<'de> Deserialize<'de> for EnvString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self::new(s))
    }
}

/// Implemented by anything that can resolve env-var names to values.
///
/// Implementers should return [`None`] for missing/unset vars. The
/// `shellexpand` integration will surface that as a typed error unless a
/// default form like `${VAR:-default}` is in play.
pub trait EnvLookup {
    fn get(&self, name: &str) -> Option<&str>;
}

impl EnvLookup for HashMap<String, String> {
    fn get(&self, name: &str) -> Option<&str> {
        HashMap::get(self, name).map(String::as_str)
    }
}

impl EnvLookup for HashMap<OsString, OsString> {
    fn get(&self, name: &str) -> Option<&str> {
        HashMap::get(self, std::ffi::OsStr::new(name)).and_then(|v| v.to_str())
    }
}

impl<T: EnvLookup + ?Sized> EnvLookup for &T {
    fn get(&self, name: &str) -> Option<&str> {
        T::get(self, name)
    }
}

#[derive(Debug, Error)]
pub enum EnvStringExpandError {
    #[error("environment variable '{var}' is not set (template: {template})")]
    MissingVar { template: Arc<str>, var: String },
}

/// Sentinel error returned by our shellexpand closure to signal a missing
/// var. shellexpand only needs it to be Display-able; we extract the
/// offending variable name from `LookupError::var_name` instead.
#[derive(Debug)]
struct MissingVar;

impl std::fmt::Display for MissingVar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("missing")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn expands_simple_var() {
        let s = EnvString::new("Bearer ${TOKEN}");
        let env = env(&[("TOKEN", "abc")]);
        assert_eq!(s.expand(&env).unwrap(), "Bearer abc");
    }

    #[test]
    fn expands_braceless_form() {
        let s = EnvString::new("$HOST/api");
        let env = env(&[("HOST", "frances.dev")]);
        assert_eq!(s.expand(&env).unwrap(), "frances.dev/api");
    }

    #[test]
    fn missing_var_errors() {
        let s = EnvString::new("Bearer ${MISSING}");
        let env = env(&[]);
        let err = s.expand(&env).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("MISSING"), "{msg}");
    }

    #[test]
    fn default_used_when_missing() {
        let s = EnvString::new("Bearer ${MISSING:-anonymous}");
        let env = env(&[]);
        assert_eq!(s.expand(&env).unwrap(), "Bearer anonymous");
    }

    #[test]
    fn default_ignored_when_present() {
        let s = EnvString::new("Bearer ${TOKEN:-anonymous}");
        let env = env(&[("TOKEN", "real")]);
        assert_eq!(s.expand(&env).unwrap(), "Bearer real");
    }

    #[test]
    fn double_dollar_escapes_dollar() {
        // shellexpand uses `$$` as the escape for a literal `$`.
        let s = EnvString::new("price: $$5");
        let env = env(&[]);
        assert_eq!(s.expand(&env).unwrap(), "price: $5");
    }

    #[test]
    fn deserializes_from_toml_string() {
        #[derive(Deserialize)]
        struct Wrap {
            value: EnvString,
        }
        let w: Wrap = toml::from_str(r#"value = "hi ${NAME}""#).unwrap();
        assert_eq!(w.value.raw(), "hi ${NAME}");
    }

    #[test]
    fn lookup_for_os_map() {
        let mut env: HashMap<OsString, OsString> = HashMap::new();
        env.insert(OsString::from("HOME"), OsString::from("/home/jono"));
        let s = EnvString::new("${HOME}/cache");
        assert_eq!(s.expand(&env).unwrap(), "/home/jono/cache");
    }
}
