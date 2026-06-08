use std::path::PathBuf;

use frances_session::runtime::{default_root_markers, discover_root};

#[tokio::test]
async fn discover_root_finds_git() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("a").join("b");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    let markers = default_root_markers();
    let root = discover_root(&sub, &markers).await;
    assert_eq!(root, dir.path());
}

#[tokio::test]
async fn discover_root_finds_jj() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("deep").join("nested");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir(dir.path().join(".jj")).unwrap();
    let markers = default_root_markers();
    let root = discover_root(&sub, &markers).await;
    assert_eq!(root, dir.path());
}

#[tokio::test]
async fn discover_root_at_cwd_when_no_marker() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("subdir");
    std::fs::create_dir_all(&sub).unwrap();
    // No markers anywhere — should fall back to the starting directory.
    let markers = vec![PathBuf::from(".nonexistent-marker")];
    let root = discover_root(&sub, &markers).await;
    assert_eq!(root, sub);
}

#[tokio::test]
async fn discover_root_cwd_is_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    let markers = default_root_markers();
    let root = discover_root(dir.path(), &markers).await;
    assert_eq!(root, dir.path());
}
