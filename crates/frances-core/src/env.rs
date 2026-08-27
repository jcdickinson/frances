//! Process environment helpers.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static INVOCATION_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    if std::env::var_os("APPIMAGE").is_some()
        && let Some(original_working_dir) = std::env::var_os("OWD")
    {
        return original_working_dir.into();
    }

    std::env::current_dir().expect("process has no current working directory")
});

/// The directory from which the user invoked Frances.
///
/// AppImage launchers may change the process working directory after saving
/// the original in `OWD`. Other launches use the process working directory.
pub fn invocation_dir() -> &'static Path {
    INVOCATION_DIR.as_path()
}
