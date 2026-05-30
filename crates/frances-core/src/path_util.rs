//! Path helpers shared across the workspace.

use std::path::{Path, PathBuf};

/// Resolve `path` against `base`. An absolute `path` is returned as-is. A
/// relative `path` is joined onto `base` when one is given, or returned
/// unchanged when there is no base.
///
/// The helper owns only this branch — callers that need a fallible base (e.g.
/// `current_dir()`) resolve it themselves and pass `Some(&base)`, so the error
/// stays at the call site rather than being silently dropped here.
pub fn resolve_relative(path: &Path, base: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match base {
        Some(b) => b.join(path),
        None => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_path_ignores_base() {
        let p = resolve_relative(Path::new("/etc/hosts"), Some(Path::new("/home")));
        assert_eq!(p, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn relative_joins_base() {
        let p = resolve_relative(Path::new("a/b"), Some(Path::new("/root")));
        assert_eq!(p, PathBuf::from("/root/a/b"));
    }

    #[test]
    fn relative_without_base_stays_relative() {
        let p = resolve_relative(Path::new("a/b"), None);
        assert_eq!(p, PathBuf::from("a/b"));
    }
}
