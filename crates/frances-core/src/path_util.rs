//! Path helpers shared across the workspace.

use std::path::{Path, PathBuf};

/// Return `true` when `descendant` is located inside `ancestor` (or is the same
/// path). Both sides are canonicalized via [`std::fs::canonicalize`] before the
/// prefix check so that symlinks are resolved. When `descendant` does not yet
/// exist on disk (e.g. a file being created), the longest existing prefix is
/// canonicalized instead and the remaining trailing components are appended.
pub fn is_within(ancestor: &Path, descendant: &Path) -> bool {
    let Ok(canonical_ancestor) = ancestor.canonicalize() else {
        return false;
    };
    let Some(canonical_descendant) = longest_existing_canonicalize(descendant) else {
        return false;
    };
    canonical_descendant.starts_with(&canonical_ancestor)
}

/// Canonicalize `path`. If the full path doesn't exist (e.g. a file about to
/// be created), walk up to the longest existing ancestor, canonicalize that,
/// then re-append the remaining trailing components. Returns `None` only when
/// no part of the path can be resolved.
fn longest_existing_canonicalize(path: &Path) -> Option<PathBuf> {
    if let Ok(c) = path.canonicalize() {
        return Some(c);
    }
    // Collect trailing components that don't exist yet.
    let mut suffix = Vec::new();
    let mut current = path;
    loop {
        if let Ok(c) = current.canonicalize() {
            let mut result = c;
            for comp in suffix.into_iter().rev() {
                result.push(comp);
            }
            return Some(result);
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                suffix.push(name);
                current = parent;
            }
            _ => return None,
        }
    }
}

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

    #[test]
    fn is_within_child() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("src").join("main.rs");
        std::fs::create_dir_all(child.parent().unwrap()).unwrap();
        std::fs::write(&child, "").unwrap();
        assert!(is_within(dir.path(), &child));
    }

    #[test]
    fn is_within_same_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_within(dir.path(), dir.path()));
    }

    #[test]
    fn is_within_sibling_is_false() {
        let parent = tempfile::tempdir().unwrap();
        let a = parent.path().join("a");
        let b = parent.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert!(!is_within(&a, &b));
    }

    #[test]
    fn is_within_via_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("file.txt");
        std::fs::write(&child, "hello").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&child, &link).unwrap();
        assert!(is_within(dir.path(), &link));
    }

    #[test]
    fn is_within_nonexistent_returns_false() {
        assert!(!is_within(
            Path::new("/definitely/does/not/exist/a"),
            Path::new("/definitely/does/not/exist/b"),
        ));
    }

    #[test]
    fn is_within_nonexistent_descendant_with_existing_parent() {
        let dir = tempfile::tempdir().unwrap();
        // File doesn't exist, but its parent directory does.
        let child = dir.path().join("new_file.txt");
        assert!(!child.exists());
        assert!(is_within(dir.path(), &child));
    }

    #[test]
    fn is_within_deeply_nonexistent_descendant() {
        let dir = tempfile::tempdir().unwrap();
        // Neither the file nor its parent directories exist, but the tempdir does.
        let child = dir.path().join("a").join("b").join("c.txt");
        assert!(!child.exists());
        assert!(is_within(dir.path(), &child));
    }
}
