//! Path helpers shared across the workspace.

use std::path::{Path, PathBuf};

/// Resolve `path` against `base`. An absolute `path` is returned as-is. A
/// relative `path` is joined onto `base` when one is given, or returned
/// unchanged when there is no base.
pub fn resolve_relative(path: &Path, base: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match base {
        Some(b) => b.join(path),
        None => path.to_path_buf(),
    }
}

/// Expand a leading `~` in `path` to the value of the `HOME` environment
/// variable. Handles bare `~` (home directory) and `~/suffix`. Returns `path`
/// unchanged when it does not start with `~`. Does **not** expand `~user`
/// forms.
pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        return home_dir();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    // ~user (or ~other) is not expanded
    path.to_path_buf()
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
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

    #[test]
    fn tilde_bare_expands_to_home() {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
        let p = expand_tilde(Path::new("~"));
        assert_eq!(p, home);
    }

    #[test]
    fn tilde_slash_expands() {
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
        let p = expand_tilde(Path::new("~/foo/bar"));
        assert_eq!(p, home.join("foo/bar"));
    }

    #[test]
    fn no_tilde_unchanged() {
        let p = expand_tilde(Path::new("foo/bar"));
        assert_eq!(p, PathBuf::from("foo/bar"));
    }

    #[test]
    fn absolute_path_unchanged() {
        let p = expand_tilde(Path::new("/usr/local/bin"));
        assert_eq!(p, PathBuf::from("/usr/local/bin"));
    }

    #[test]
    fn tilde_user_not_expanded() {
        let p = expand_tilde(Path::new("~root"));
        assert_eq!(p, PathBuf::from("~root"));
    }

    #[test]
    fn tilde_user_slash_not_expanded() {
        let p = expand_tilde(Path::new("~root/docs"));
        assert_eq!(p, PathBuf::from("~root/docs"));
    }
}
