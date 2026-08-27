//! Process environment helpers.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static INVOCATION_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    if std::env::var_os("APPDIR").is_some()
        && let Some(invocation_dir) =
            appimage_invocation_dir(std::env::var_os("OWD"), std::env::var_os("PWD"))
    {
        return invocation_dir;
    }

    std::env::current_dir().expect("process has no current working directory")
});

fn appimage_invocation_dir(
    original_working_dir: Option<std::ffi::OsString>,
    inherited_working_dir: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let original_working_dir = original_working_dir
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    let inherited_working_dir = inherited_working_dir
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());

    original_working_dir.or(inherited_working_dir)
}

/// The directory from which the user invoked Frances.
///
/// AppImage launchers may change the process working directory after saving
/// the original in `OWD` or leaving it in `PWD`. Other launches use the
/// process working directory.
pub fn invocation_dir() -> &'static Path {
    INVOCATION_DIR.as_path()
}

#[cfg(test)]
mod tests {
    use super::appimage_invocation_dir;

    #[test]
    fn appimage_uses_inherited_working_dir_when_owd_is_missing() {
        let resolved = appimage_invocation_dir(None, Some("/home/user/project".into())).unwrap();

        assert_eq!(resolved, std::path::Path::new("/home/user/project"));
    }
}
