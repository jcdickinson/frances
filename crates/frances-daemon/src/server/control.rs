use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tracing::{trace, warn};

use crate::protocol::{DaemonPid, DaemonStatus, PROTOCOL_VERSION, SessionId};

use super::{ServerError, ServerState};

const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

// The control socket speaks a deliberately tiny newline-delimited TEXT protocol,
// not tarpc/bincode. Rationale: control is for management — "what version are
// you?" and "please shut down" — and it has to keep working *across binary
// versions* so a client built against a new schema can still ask an old daemon
// to step aside. Any binary serialization format (bincode, protobuf, etc.)
// breaks the moment a single field shape changes, which is exactly the
// situation the version-mismatch flow exists to handle. Plain text with
// `key=value` lines and an explicit terminator is forward-compatible by
// convention: unknown commands → `err`; unknown keys → ignored.
//
// On every accepted connection the server's first action is to write the
// current build's PROTOCOL_VERSION as a hex banner line. The client reads
// that line first and can decide to bail without sending any command.
//
// Wire shape:
//   server → client: "<protocol_version_hex>\n"
//   client → server: "<command>[ <args>]\n"
//   server → client: "ok\n" or "err <msg>\n"
//                    optional "key=value\n" lines
//                    "\n"  (blank line ends response)
pub(super) async fn serve_control(
    path: PathBuf,
    state: Arc<ServerState>,
) -> std::result::Result<(), ServerError> {
    let listener = UnixListener::bind(&path).map_err(|source| ServerError::BindSocket {
        label: "control",
        path: path.clone(),
        source,
    })?;
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                warn!(%error, "control accept error");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_control_conn(&mut stream, state).await {
                trace!(%error, "control handler exited");
            }
        });
    }
}

async fn handle_control_conn(
    stream: &mut UnixStream,
    state: Arc<ServerState>,
) -> std::result::Result<(), ServerError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = stream.split();

    // Greet with our build's protocol id so the client can decide compatibility
    // before issuing any command.
    write_half
        .write_all(format!("{PROTOCOL_VERSION:016x}\n").as_bytes())
        .await
        .map_err(ServerError::ControlIo)?;
    write_half.flush().await.map_err(ServerError::ControlIo)?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    if reader
        .read_line(&mut line)
        .await
        .map_err(ServerError::ControlIo)?
        == 0
    {
        return Ok(());
    }
    let request = line.trim();
    let mut parts = request.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let args = parts.next().unwrap_or("").trim();

    let response = match cmd {
        "ping" => "ok\n\n".to_string(),
        "status" => {
            let s = daemon_status(&state);
            let mut out = String::from("ok\n");
            out.push_str(&format!("session_id={}\n", s.session_id));
            out.push_str(&format!("client_attached={}\n", s.client_attached));
            out.push_str(&format!("daemon_pid={}\n", s.daemon_pid));
            out.push_str(&format!("protocol_version={:016x}\n", s.protocol_version));
            out.push('\n');
            out
        }
        "stop" => {
            let _delete_state = args.split_whitespace().any(|tok| tok == "delete=1");
            let state = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SHUTDOWN_GRACE).await;
                state.shutdown.notify_waiters();
            });
            "ok\n\n".to_string()
        }
        other => format!("err unknown command: {other}\n\n"),
    };

    write_half
        .write_all(response.as_bytes())
        .await
        .map_err(ServerError::ControlIo)?;
    write_half.flush().await.map_err(ServerError::ControlIo)?;
    Ok(())
}

fn daemon_status(state: &ServerState) -> DaemonStatus {
    DaemonStatus {
        session_id: SessionId(state.session.id.clone()),
        client_attached: *state.client_attached.lock(),
        daemon_pid: DaemonPid(state.daemon_pid),
        control_socket_path: state.session.control_socket_path(),
        client_socket_path: state.session.client_socket_path(),
        events_socket_path: state.session.events_socket_path(),
        protocol_version: PROTOCOL_VERSION,
    }
}
