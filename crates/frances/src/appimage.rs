use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use tauri::Manager;

const WORKER_IMAGES_DIR: &str = "worker-images";

pub struct AppImageResourceResolver {
    resource_dir: PathBuf,
    cache_dir: PathBuf,
}

impl AppImageResourceResolver {
    pub fn discover(app: &tauri::AppHandle) -> Result<Option<Self>> {
        if std::env::var_os("APPDIR").is_none() {
            return Ok(None);
        }

        let resource_dir = app
            .path()
            .resource_dir()
            .context("resolve AppImage resource directory")?;
        let cache_dir = app
            .path()
            .app_cache_dir()
            .context("resolve Frances cache directory")?;
        Ok(Some(Self {
            resource_dir,
            cache_dir,
        }))
    }

    fn resource_image(&self, target: &str) -> PathBuf {
        self.resource_dir
            .join(WORKER_IMAGES_DIR)
            .join(target)
            .join("frances-worker.gz")
    }

    fn cached_worker(&self, target: &str) -> PathBuf {
        self.cache_dir
            .join(WORKER_IMAGES_DIR)
            .join(target)
            .join("frances-worker")
    }

    pub async fn worker_image(&self, target: &str) -> Result<PathBuf> {
        let source_path = self.resource_image(target);
        let worker_path = self.cached_worker(target);
        let worker_dir = worker_path
            .parent()
            .context("worker cache path has no parent directory")?;
        tokio::fs::create_dir_all(worker_dir)
            .await
            .with_context(|| format!("create worker cache directory {}", worker_dir.display()))?;

        let image = tokio::fs::read(&source_path)
            .await
            .with_context(|| format!("open AppImage worker resource {}", source_path.display()))?;

        // Worker artifacts stay compressed through CI and AppImage assembly because
        // linuxdeploy rewrites embedded ELF files, corrupting static musl PIE binaries.
        let decompression_path = source_path.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let mut decoder = GzDecoder::new(image.as_slice());
            let mut worker = Vec::new();
            decoder.read_to_end(&mut worker).with_context(|| {
                format!(
                    "decompress AppImage worker resource {}",
                    decompression_path.display()
                )
            })?;
            Ok::<_, anyhow::Error>(worker)
        })
        .await
        .context("join AppImage worker decompression task")??;

        let temporary_path = worker_path.with_extension(format!("tmp-{}", std::process::id()));
        let write_result = write_worker(&worker, &temporary_path, &worker_path).await;
        if write_result.is_err()
            && let Err(error) = tokio::fs::remove_file(&temporary_path).await
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                ?error,
                path = %temporary_path.display(),
                "failed to remove temporary worker image"
            );
        }
        write_result?;

        Ok(worker_path)
    }
}

async fn write_worker(worker: &[u8], temporary_path: &Path, worker_path: &Path) -> Result<()> {
    tokio::fs::write(temporary_path, worker)
        .await
        .with_context(|| format!("write worker image {}", temporary_path.display()))?;
    tokio::fs::set_permissions(temporary_path, std::fs::Permissions::from_mode(0o755))
        .await
        .with_context(|| format!("make worker image executable {}", temporary_path.display()))?;
    tokio::fs::rename(temporary_path, worker_path)
        .await
        .with_context(|| {
            format!(
                "install worker image {} at {}",
                temporary_path.display(),
                worker_path.display()
            )
        })?;
    Ok(())
}

pub async fn local_worker_image(app: &tauri::AppHandle) -> Result<Option<PathBuf>> {
    let Some(resources) = AppImageResourceResolver::discover(app)? else {
        return Ok(None);
    };

    let target = match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        architecture => bail!("AppImage has no worker image for Linux architecture {architecture}"),
    };

    resources.worker_image(target).await.map(Some)
}

pub fn launcher_executable(fallback: &Path) -> PathBuf {
    // NixOS appimage-run has already extracted the image and provided its FHS
    // environment. Re-executing the AppImage from inside that environment
    // bypasses the wrapper and fails, while the extracted binary remains at a
    // stable cache path.
    if std::env::var_os("APPIMAGE_SILENT_INSTALL").is_some() {
        return fallback.to_path_buf();
    }

    std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::AppImageResourceResolver;

    #[test]
    fn resolves_worker_image_below_the_resource_directory() {
        let resolver = AppImageResourceResolver {
            resource_dir: "/tmp/frances-resources".into(),
            cache_dir: "/tmp/frances-cache".into(),
        };

        assert_eq!(
            resolver.resource_image("aarch64-apple-darwin"),
            std::path::Path::new(
                "/tmp/frances-resources/worker-images/aarch64-apple-darwin/frances-worker.gz"
            )
        );
        assert_eq!(
            resolver.cached_worker("aarch64-apple-darwin"),
            std::path::Path::new(
                "/tmp/frances-cache/worker-images/aarch64-apple-darwin/frances-worker"
            )
        );
    }

    #[tokio::test]
    async fn materializes_an_executable_worker() {
        let test_dir =
            std::env::temp_dir().join(format!("frances-appimage-test-{}", std::process::id()));
        let resolver = AppImageResourceResolver {
            resource_dir: test_dir.join("resources"),
            cache_dir: test_dir.join("cache"),
        };
        let resource_image = resolver.resource_image("x86_64-unknown-linux-musl");
        fs::create_dir_all(resource_image.parent().unwrap()).unwrap();
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"worker bytes").unwrap();
        fs::write(&resource_image, encoder.finish().unwrap()).unwrap();

        let worker = resolver
            .worker_image("x86_64-unknown-linux-musl")
            .await
            .unwrap();

        assert_eq!(fs::read(&worker).unwrap(), b"worker bytes");
        assert_ne!(
            fs::metadata(&worker).unwrap().permissions().mode() & 0o111,
            0
        );
        fs::remove_dir_all(test_dir).unwrap();
    }
}
