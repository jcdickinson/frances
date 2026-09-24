use std::{collections::HashMap, ffi::OsString, io, process::Stdio};

use frances_config::EnvString;
use frances_worker_protocol::{Content, ErrorCode, Feed, ProcessOptions, ResponseError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) async fn open(
    options: ProcessOptions,
    mut input: Feed<Content>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<(Feed<Content>, impl Future<Output = ()> + Send), ResponseError> {
    let inherited: HashMap<OsString, OsString> = std::env::vars_os().collect();
    let mut env = inherited.clone();
    for (key, template) in options.env {
        let value = EnvString::new(template)
            .expand(&inherited)
            .map_err(|error| {
                let frances_config::EnvStringExpandError::MissingVar { var, .. } = error;
                ResponseError::new(
                    ErrorCode::InvalidRequest,
                    format!("worker environment variable is unavailable: {var}"),
                )
            })?;
        env.insert(key.into(), value.into());
    }
    let cwd = std::env::current_dir()
        .map_err(process_error)?
        .join(options.cwd);
    let executable = frances_core::which::which_in(
        &options.command,
        env.get(std::ffi::OsStr::new("PATH")).cloned(),
        &cwd,
    )
    .await
    .map_err(|error| {
        ResponseError::new(
            ErrorCode::NotFound,
            format!("cannot find worker executable {}: {error}", options.command),
        )
    })?;
    let mut child = tokio::process::Command::new(executable)
        .args(options.args)
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(process_error)?;
    let mut stdin = child.stdin.take().expect("piped process stdin");
    let mut stdout = child.stdout.take().expect("piped process stdout");
    let (sender, output) = Feed::channel();
    let grace = std::time::Duration::from_secs(options.shutdown_timeout_seconds);
    let task = async move {
        let write = async {
            while let Some(content) = input.next().await.map_err(io::Error::other)? {
                content.copy_to(&mut stdin).await?;
                stdin.flush().await?;
            }
            Ok::<_, io::Error>(())
        };
        let read = async {
            let mut bytes = vec![0; 16 * 1024];
            loop {
                let count = stdout.read(&mut bytes).await?;
                if count == 0 {
                    return Ok::<_, io::Error>(());
                }
                sender
                    .send(Content::from_bytes(bytes[..count].to_vec()))
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "process output receiver closed")
                    })?;
            }
        };
        let result = tokio::select! {
            result = write => result,
            result = read => result,
            () = sender.closed() => Ok(()),
            () = cancel.cancelled() => Ok(()),
        };
        if let Err(error) = result {
            tracing::debug!(%error, "worker process stream closed");
        }
        if let Err(error) = frances_core::process::shutdown(&mut child, grace).await {
            tracing::warn!(%error, "failed to stop worker process");
        }
    };
    Ok((output, task))
}

fn process_error(error: io::Error) -> ResponseError {
    ResponseError::new(ErrorCode::Io, error.to_string())
}
