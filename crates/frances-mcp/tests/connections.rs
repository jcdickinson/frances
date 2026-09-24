use frances_config::EnvString;
use frances_mcp::{Connection, Environment, Error, Lifecycle, ServerConfig, Transport};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

fn fixture() -> String {
    format!("{}/tests/support/server.py", env!("CARGO_MANIFEST_DIR"))
}

fn config(mode: &str, log: &std::path::Path) -> ServerConfig {
    ServerConfig {
        shutdown_timeout_seconds: 5,
        transport: Transport::LocalStdio {
            command: "python3".into(),
            args: vec![fixture(), mode.into(), log.display().to_string()],
            env: BTreeMap::from([("MCP_FIXTURE_ENV".into(), EnvString::new("present"))]),
            cwd: None,
        },
        lifecycle: Lifecycle::Auto,
        startup_timeout_seconds: 15,
        request_timeout_seconds: 3,
    }
}

fn environment(temp: &tempfile::TempDir) -> Environment {
    Environment {
        worker: None,
        cwd: temp.path().to_path_buf(),
        env: Arc::new(std::env::vars_os().collect()),
    }
}

fn messages(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_resolves_commands_in_effective_path_and_working_directory() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(
        frances_core::which::which_in("python3", std::env::var_os("PATH"), temp.path())
            .await
            .unwrap(),
        bin.join("mcp-fixture"),
    )
    .unwrap();
    for mode in ["inherited", "override", "relative", "missing"] {
        let mut config = config("modern", &temp.path().join(format!("{mode}.jsonl")));
        let Transport::LocalStdio {
            command, env, cwd, ..
        } = &mut config.transport
        else {
            unreachable!()
        };
        *command = if mode == "relative" {
            "./mcp-fixture"
        } else {
            "mcp-fixture"
        }
        .into();
        let mut environment = environment(&temp);
        Arc::make_mut(&mut environment.env)
            .insert("PATH".into(), temp.path().join("absent").into_os_string());
        match mode {
            "inherited" => {
                Arc::make_mut(&mut environment.env)
                    .insert("PATH".into(), bin.clone().into_os_string());
            }
            "override" => {
                env.insert("PATH".into(), EnvString::new("bin"));
            }
            "relative" => {
                *cwd = Some("bin".into());
            }
            _ => {}
        }
        let result = Connection::connect(
            "fixture".into(),
            &config,
            &environment,
            &CancellationToken::new(),
        )
        .await;
        if mode == "missing" {
            assert!(matches!(result, Err(Error::Executable { .. })));
        } else {
            result.unwrap().close().await;
        }
    }
}

#[tokio::test]
async fn modern_stdio_and_classic_fallback_preserve_content_and_pagination() {
    for mode in ["modern", "legacy"] {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("requests.jsonl");
        let cancel = CancellationToken::new();
        let connection = Connection::connect(
            "fixture".into(),
            &config(mode, &log),
            &environment(&temp),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(connection.tools(&cancel).await.unwrap().len(), 5);
        let result = connection
            .call(
                "echo".into(),
                json!({"text":"hello"}).as_object().unwrap().clone(),
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(
            result.structured_content.as_ref().unwrap()["env"],
            "present"
        );
        assert!(result.meta.is_some());
        assert_eq!(connection.resources(&cancel).await.unwrap().len(), 1);
        assert_eq!(connection.templates(&cancel).await.unwrap().len(), 1);
        assert_eq!(
            connection
                .read("fixture://readme".into(), &cancel)
                .await
                .unwrap()
                .contents
                .len(),
            1
        );
        assert_eq!(connection.prompts(&cancel).await.unwrap().len(), 1);
        assert_eq!(
            connection
                .prompt(
                    "review".into(),
                    BTreeMap::from([("topic".into(), "review this".into())]),
                    &cancel
                )
                .await
                .unwrap()
                .messages
                .len(),
            1
        );
        assert_eq!(
            connection
                .call(
                    "fail".into(),
                    json!({"text":"failed"}).as_object().unwrap().clone(),
                    &cancel
                )
                .await
                .unwrap()
                .is_error,
            Some(true)
        );
        connection.close().await;
        let requests = messages(&log);
        assert_eq!(requests[0]["method"], "server/discover");
        assert_eq!(
            requests.iter().any(|r| r["method"] == "initialize"),
            mode == "legacy"
        );
        if mode == "modern" {
            let call = requests
                .iter()
                .find(|r| r["method"] == "tools/call")
                .unwrap();
            assert_eq!(
                call["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
                "2026-07-28"
            );
            assert!(
                call["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]
                    .is_null()
            );
        }
    }
}

#[tokio::test]
async fn cancellation_reaches_server_and_continuations_are_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("requests.jsonl");
    let cancel = CancellationToken::new();
    let connection = Connection::connect(
        "fixture".into(),
        &config("modern", &log),
        &environment(&temp),
        &cancel,
    )
    .await
    .unwrap();
    let child = cancel.child_token();
    let call = connection.call(
        "slow".into(),
        json!({"text":"wait"}).as_object().unwrap().clone(),
        &child,
    );
    let interrupt = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        child.cancel();
    };
    let (result, ()) = tokio::join!(call, interrupt);
    assert!(matches!(result, Err(Error::Interrupted)));
    connection
        .call(
            "continue".into(),
            json!({"text":"done"}).as_object().unwrap().clone(),
            &cancel,
        )
        .await
        .unwrap();
    assert!(matches!(
        connection
            .call("forever".into(), serde_json::Map::new(), &cancel)
            .await,
        Err(Error::ContinuationLimit)
    ));
    connection.close().await;
    let requests = messages(&log);
    assert!(
        requests
            .iter()
            .any(|r| r["method"] == "notifications/cancelled")
    );
    let calls: Vec<_> = requests
        .iter()
        .filter(|r| r["params"]["name"] == "continue")
        .collect();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0]["id"], calls[1]["id"]);
    assert_eq!(calls[1]["params"]["requestState"], "opaque");
}

#[tokio::test]
async fn streamable_http_uses_modern_discovery() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("requests.jsonl");
    let mut process = tokio::process::Command::new("python3")
        .args([
            fixture(),
            "modern".into(),
            log.display().to_string(),
            "http".into(),
        ])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(process.stdout.take().unwrap());
    let mut port = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut port))
        .await
        .unwrap()
        .unwrap();
    let mut config = config("modern", &log);
    config.transport = Transport::Http {
        url: format!("http://127.0.0.1:{}/mcp", port.trim())
            .parse()
            .unwrap(),
        headers: BTreeMap::new(),
    };
    let cancel = CancellationToken::new();
    let connection = Connection::connect("http".into(), &config, &environment(&temp), &cancel)
        .await
        .unwrap();
    assert_eq!(connection.tools(&cancel).await.unwrap().len(), 5);
    connection
        .call(
            "echo".into(),
            json!({"text":"http"}).as_object().unwrap().clone(),
            &cancel,
        )
        .await
        .unwrap();
    connection.close().await;
    process.kill().await.unwrap();
    process.wait().await.unwrap();
    assert!(!messages(&log).iter().any(|r| r["method"] == "initialize"));
}

#[tokio::test]
async fn duplicate_tools_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let connection = Connection::connect(
        "fixture".into(),
        &config("duplicate", &temp.path().join("log")),
        &environment(&temp),
        &cancel,
    )
    .await
    .unwrap();
    assert!(matches!(
        connection.tools(&cancel).await,
        Err(Error::DuplicateTool(_))
    ));
    connection.close().await;
}

#[tokio::test]
async fn modern_tool_subscription_invalidates_catalog() {
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let connection = Connection::connect(
        "fixture".into(),
        &config("notify", &temp.path().join("log")),
        &environment(&temp),
        &cancel,
    )
    .await
    .unwrap();
    let revision = connection.catalog_revision();
    connection
        .call(
            "changed".into(),
            json!({"text":"change"}).as_object().unwrap().clone(),
            &cancel,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while connection.catalog_revision() == revision {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    connection.close().await;
}

#[tokio::test]
async fn stdio_mcp_runs_through_worker_without_host_path_lookup() {
    let temp = tempfile::tempdir().unwrap();
    let (host, remote) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(frances_worker::serve(remote));
    let worker = frances_worker::Client::connect(host).await.unwrap();
    let mut env = environment(&temp);
    env.worker = Some(worker.clone());
    Arc::make_mut(&mut env.env).insert("PATH".into(), "/no/host/executables".into());
    let mut config = config("modern", &temp.path().join("worker.jsonl"));
    let Transport::LocalStdio {
        command,
        args,
        env: overrides,
        cwd,
    } = config.transport
    else {
        unreachable!()
    };
    config.transport = Transport::Stdio {
        command,
        args,
        env: overrides,
        cwd,
    };
    let cancel = CancellationToken::new();
    let connection = Connection::connect("worker".into(), &config, &env, &cancel)
        .await
        .unwrap();
    assert_eq!(connection.tools(&cancel).await.unwrap().len(), 5);
    let result = connection
        .call(
            "echo".into(),
            json!({"text":"worker MCP"}).as_object().unwrap().clone(),
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(result.structured_content.unwrap()["env"], "present");
    connection.close().await;
    worker.shutdown().await.unwrap();
    server.await.unwrap().unwrap();
}
