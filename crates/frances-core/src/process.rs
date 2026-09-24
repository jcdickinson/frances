use std::{io, time::Duration};
use tokio::process::Child;

/// Give a child time to terminate, then kill and reap it. Windows has no SIGTERM.
pub async fn shutdown(child: &mut Child, grace: Duration) -> io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SAFETY: kill takes a process ID and signal, with no pointer arguments.
            if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
                let error = io::Error::last_os_error();
                tracing::debug!(%error, pid, "sending SIGTERM failed");
            }
        }
        match tokio::time::timeout(grace, child.wait()).await {
            Ok(result) => return result.map(|_| ()),
            Err(_) => tracing::debug!("process did not exit before its shutdown deadline"),
        }
    }
    #[cfg(not(unix))]
    let _ = grace;
    child.kill().await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn shutdown_allows_sigterm_handler_and_escalates_after_timeout() {
        let python = crate::which::which_in(
            "python3",
            std::env::var_os("PATH"),
            std::env::current_dir().unwrap(),
        )
        .await
        .unwrap();
        for ignore in [false, true] {
            let handler = if ignore {
                "signal.SIG_IGN"
            } else {
                "lambda *_: sys.exit(0)"
            };
            let script = format!(
                "import signal,sys,time; signal.signal(signal.SIGTERM, {handler}); print('ready', flush=True); time.sleep(60)"
            );
            let mut child = tokio::process::Command::new(&python)
                .args(["-c", &script])
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut ready = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut ready)
                .await
                .unwrap();
            assert_eq!(ready.trim(), "ready");
            let start = tokio::time::Instant::now();
            let grace = Duration::from_secs(1);
            shutdown(&mut child, grace).await.unwrap();
            let status = child.try_wait().unwrap().unwrap();
            if ignore {
                assert!(start.elapsed() >= grace);
                assert_eq!(status.signal(), Some(libc::SIGKILL));
            } else {
                assert!(status.success());
            }
        }
    }
}
