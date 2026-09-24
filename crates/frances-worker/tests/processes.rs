#![cfg(unix)]

use frances_worker::Client;
use frances_worker_protocol::ProcessOptions;
use std::{collections::BTreeMap, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn process_uses_worker_environment_and_streams_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let python = frances_core::which::which_in("python3", std::env::var_os("PATH"), temp.path())
        .await
        .unwrap();
    std::os::unix::fs::symlink(python, bin.join("worker-only-python")).unwrap();
    let mut worker = tokio::process::Command::new(env!("CARGO_BIN_EXE_frances-worker"))
        .args(["serve", "--stdio"])
        .env("PATH", &bin)
        .env("FRANCES_WORKER_TEST_VALUE", "from-worker")
        .current_dir(temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let client = Client::connect(tokio::io::join(
        worker.stdout.take().unwrap(),
        worker.stdin.take().unwrap(),
    ))
    .await
    .unwrap();
    let script = r#"import os, sys
print(os.getenv('EXPANDED'))
print(os.getcwd())
print(os.getpid())
while True:
    data = os.read(0, 4096)
    if not data:
        break
    os.write(1, data)
"#;
    let options = || ProcessOptions {
        shutdown_timeout_seconds: 5,
        command: "worker-only-python".into(),
        args: vec!["-u".into(), "-c".into(), script.into()],
        env: BTreeMap::from([
            ("EXPANDED".into(), "${FRANCES_WORKER_TEST_VALUE}".into()),
            ("PATH".into(), "${PATH}".into()),
        ]),
        cwd: ".".into(),
    };
    let stream = client.open_process(options()).await.unwrap();
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    stream.read_line(&mut line).await.unwrap();
    assert_eq!(line.trim(), "from-worker");
    line.clear();
    stream.read_line(&mut line).await.unwrap();
    assert_eq!(line.trim(), temp.path().to_str().unwrap());
    line.clear();
    stream.read_line(&mut line).await.unwrap();
    let pid: u32 = line.trim().parse().unwrap();
    let payload = vec![0xA5; 256 * 1024];
    let (mut read, mut write) = tokio::io::split(stream);
    let mut received = vec![0; payload.len()];
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::try_join!(write.write_all(&payload), read.read_exact(&mut received))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(received, payload);
    drop(read);
    drop(write);
    #[cfg(target_os = "linux")]
    tokio::time::timeout(Duration::from_secs(5), async {
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    #[cfg(not(target_os = "linux"))]
    let _ = pid;
    let mut missing = options();
    missing
        .env
        .insert("PATH".into(), "/no/executables/here".into());
    assert!(client.open_process(missing).await.is_err());
    let mut active = BufReader::new(client.open_process(options()).await.unwrap());
    for _ in 0..3 {
        line.clear();
        active.read_line(&mut line).await.unwrap();
    }
    let active_pid: u32 = line.trim().parse().unwrap();
    client.shutdown().await.unwrap();
    #[cfg(target_os = "linux")]
    tokio::time::timeout(Duration::from_secs(5), async {
        while std::path::Path::new(&format!("/proc/{active_pid}")).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    #[cfg(not(target_os = "linux"))]
    let _ = active_pid;
    drop(active);
    drop(client);
    // Tokio stdin may retain a blocking read after protocol shutdown.
    worker.kill().await.unwrap();
}
