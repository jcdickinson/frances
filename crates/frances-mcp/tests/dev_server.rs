//! Exercise the development server through the same client used by Frances.
use frances_mcp::{Connection, Environment, Lifecycle, ServerConfig, Transport};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn deno_dev_server_supports_stdio_and_http() {
    let source = format!(
        "{}/../../packages/frances-dev-mcp/src/main.ts",
        env!("CARGO_MANIFEST_DIR")
    );
    let temp = tempfile::tempdir().unwrap();
    let environment = Environment {
        worker: None,
        cwd: temp.path().to_path_buf(),
        env: Arc::new(std::env::vars_os().collect()),
    };
    for http in [false, true] {
        let state = temp
            .path()
            .join(if http { "http.json" } else { "stdio.json" });
        let args = vec![
            "run".to_string(),
            "--no-config".into(),
            "--allow-read".into(),
            "--allow-write".into(),
            "--allow-net=127.0.0.1".into(),
            source.clone(),
            "--state".into(),
            state.display().to_string(),
        ];
        let mut process = None;
        let transport = if http {
            let mut child = tokio::process::Command::new("deno")
                .args(&args)
                .args(["--http", "--port", "0"])
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
            let url = tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let line = lines.next_line().await.unwrap().expect("server exited");
                    if let Some(url) = line.strip_prefix("frances-dev-mcp: ") {
                        break url.parse().unwrap();
                    }
                    eprintln!("{line}");
                }
            })
            .await
            .unwrap();
            process = Some(child);
            Transport::Http {
                url,
                headers: BTreeMap::new(),
            }
        } else {
            Transport::LocalStdio {
                command: "deno".into(),
                args,
                env: BTreeMap::new(),
                cwd: None,
            }
        };
        let config = ServerConfig {
            transport,
            lifecycle: Lifecycle::Modern,
            shutdown_timeout_seconds: 5,
            startup_timeout_seconds: 15,
            request_timeout_seconds: 5,
        };
        let cancel = CancellationToken::new();
        let connection = Connection::connect("dev".into(), &config, &environment, &cancel)
            .await
            .unwrap();
        assert_eq!(connection.tools(&cancel).await.unwrap().len(), 4);
        let result = connection
            .call(
                "increment".into(),
                json!({}).as_object().unwrap().clone(),
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(result.structured_content.unwrap()["counter"], 1);
        assert_eq!(connection.resources(&cancel).await.unwrap().len(), 1);
        connection.close().await;
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(state).unwrap()).unwrap();
        assert_eq!(saved["counter"], 1);
        if let Some(mut child) = process {
            child.kill().await.unwrap();
        }
    }
}
