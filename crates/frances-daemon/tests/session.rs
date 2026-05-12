use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use frances_daemon::{session::*, tty::TtyKey};

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

#[test]
fn metadata_roundtrip_and_session_creation() {
    let root = temp_root("metadata");
    let paths = Paths {
        state_root: root.join("state"),
        runtime_root: root.join("runtime"),
    };

    let session = paths
        .create_session(Some(PathBuf::from("/tmp/work")))
        .expect("create session");

    let loaded = paths.load_session(&session.id).expect("load session");
    assert_eq!(loaded.meta.id, session.id);
    assert_eq!(loaded.meta.cwd, Some(PathBuf::from("/tmp/work")));
    assert_eq!(
        fs::metadata(&loaded.dir).expect("metadata").mode() & 0o777,
        0o700
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn dangling_tty_link_is_cleaned_up() {
    let root = temp_root("tty");
    let paths = Paths {
        state_root: root.join("state"),
        runtime_root: root.join("runtime"),
    };
    paths.ensure_layout().expect("layout");

    let missing_target = paths.sessions_root().join("missing-session");
    let tty_key = TtyKey("tty-key".into());
    symlink(&missing_target, paths.tty_link_path(&tty_key)).expect("create link");

    let resolved = paths.resolve_tty_link(&tty_key).expect("resolve tty");
    assert!(resolved.is_none());
    assert!(!paths.tty_link_path(&tty_key).exists());

    let _ = fs::remove_dir_all(root);
}
