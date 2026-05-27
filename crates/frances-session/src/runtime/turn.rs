use std::sync::Arc;

use tracing::warn;

use crate::Result;
use crate::events::StreamFrame;
use crate::workflows;

use super::SessionRuntime;

pub(super) async fn run_prompt(runtime: Arc<SessionRuntime>, text: String) {
    if let Err(error) = stream_prompt(&runtime, text).await {
        warn!(%error, "prompt handler failed");
        runtime.events.send(StreamFrame::Error(format!("{error}")));
    }
    runtime.events.send(StreamFrame::Done);
}

async fn stream_prompt(runtime: &Arc<SessionRuntime>, text: String) -> Result<()> {
    let result = workflows::cycle(runtime, &text).await;
    if let Err(error) = runtime
        .editor_factory
        .session
        .lock()
        .await
        .commit_edits()
        .await
    {
        warn!(%error, "edit_session::commit_edits failed");
    }
    result
}
