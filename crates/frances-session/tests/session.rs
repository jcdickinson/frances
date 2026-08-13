use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use frances_session::session::*;
use frances_session::workspace::{Workspace, WorkspaceSource};

fn temp_root(label: &str) -> PathBuf {
    let unique = format!(
        "frances-test-{}-{}-{}",
        label,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    std::env::temp_dir().join(unique)
}

fn temp_paths(root: &Path) -> Paths {
    Paths {
        state_root: root.join("state"),
        runtime_root: root.join("runtime"),
    }
}

#[test]
fn metadata_roundtrip_and_session_creation() {
    let root = temp_root("metadata");
    let work = root.join("work");
    fs::create_dir_all(&work).expect("create work dir");
    let paths = temp_paths(&root);
    let workspace = Workspace::open(&work).expect("open workspace");

    let session = paths.create_session(&workspace).expect("create session");

    let loaded = paths.load_session(&session.id).expect("load session");
    assert_eq!(loaded.meta.id, session.id);
    assert_eq!(loaded.meta.cwd, workspace.primary_dir());
    assert_eq!(
        loaded.meta.workspace_source,
        workspace.source.identity_path()
    );
    assert_eq!(
        fs::metadata(&loaded.dir).expect("metadata").mode() & 0o777,
        0o700
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn each_launch_creates_distinct_session() {
    let root = temp_root("distinct");
    let work = root.join("work");
    fs::create_dir_all(&work).expect("create work dir");
    let paths = temp_paths(&root);
    let workspace = Workspace::open(&work).expect("open workspace");

    let first = paths.create_session(&workspace).expect("first session");
    let second = paths.create_session(&workspace).expect("second session");

    assert_ne!(first.id, second.id);
    assert_ne!(first.dir, second.dir);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dir_workspace_canonicalizes() {
    let root = temp_root("canonical");
    let work = root.join("work");
    fs::create_dir_all(&work).expect("create work dir");

    let direct = Workspace::open(&work).expect("open direct");
    let indirect = Workspace::open(&work.join("..").join("work")).expect("open indirect");

    assert_eq!(direct.source, indirect.source);
    assert_eq!(direct.dirs(), indirect.dirs());
    assert!(matches!(direct.source, WorkspaceSource::Dir(_)));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_file_resolves_relative_dirs() {
    let root = temp_root("wsfile");
    let a = root.join("a");
    let b = root.join("nested").join("b");
    fs::create_dir_all(&a).expect("create a");
    fs::create_dir_all(&b).expect("create b");
    let file = root.join("ws.json");
    fs::write(&file, r#"{ "dirs": ["a", "nested/b"] }"#).expect("write file");

    let workspace = Workspace::open(&file).expect("open workspace file");

    assert!(matches!(workspace.source, WorkspaceSource::File(_)));
    assert_eq!(workspace.primary_dir(), a.canonicalize().unwrap());
    assert_eq!(
        workspace.dirs(),
        [a.canonicalize().unwrap(), b.canonicalize().unwrap()]
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_file_with_no_dirs_errors() {
    let root = temp_root("nodirs");
    fs::create_dir_all(&root).expect("create root");
    let file = root.join("ws.json");
    fs::write(&file, r#"{ "dirs": [] }"#).expect("write file");

    assert!(Workspace::open(&file).is_err());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_file_with_missing_dir_errors() {
    let root = temp_root("missingdir");
    fs::create_dir_all(&root).expect("create root");
    let file = root.join("ws.json");
    fs::write(&file, r#"{ "dirs": ["does-not-exist"] }"#).expect("write file");

    assert!(Workspace::open(&file).is_err());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn missing_path_errors() {
    let root = temp_root("missing");

    assert!(Workspace::open(&root.join("nope")).is_err());
}
