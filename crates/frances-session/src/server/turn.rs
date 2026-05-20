use std::sync::Arc;

use tokio::net::UnixStream;
use tracing::{trace, warn};

use crate::Result;
use crate::protocol::StreamFrame;
use crate::transport::write_message;
use crate::workflows;

use super::ServerState;

pub(super) async fn run_prompt(state: Arc<ServerState>, stream: &mut UnixStream, text: String) {
    if let Err(error) = stream_prompt(&state, stream, text).await {
        warn!(%error, "prompt handler failed");
        match write_message(stream, &StreamFrame::Error(format!("{error}"))).await {
            Ok(()) => trace!("wrote error frame"),
            Err(e) => warn!(error = %e, "failed to write error frame"),
        }
    }
    match write_message(stream, &StreamFrame::Done).await {
        Ok(()) => trace!("wrote done frame"),
        Err(e) => warn!(error = %e, "failed to write done frame"),
    }
}

async fn stream_prompt(
    state: &Arc<ServerState>,
    stream: &mut UnixStream,
    text: String,
) -> Result<()> {
    let result = workflows::cycle(state, stream, &text).await;
    if let Err(error) = state.editor_factory.session.lock().await.end_turn().await {
        warn!(%error, "edit_session::end_turn failed");
    }
    result
}
