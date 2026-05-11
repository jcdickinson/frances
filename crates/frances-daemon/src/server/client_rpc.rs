use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use tarpc::context;
use tarpc::server::Channel;
use tarpc::tokio_serde::formats::Bincode;
use tracing::{trace, warn};

use crate::context::InvocationContext;
use crate::protocol::{AttachResponse, Client, PromptId, SessionId};

use super::turn::run_prompt;
use super::{ServerError, ServerState};

#[derive(Clone)]
struct ClientServer {
    state: Arc<ServerState>,
}

impl Client for ClientServer {
    async fn attach(self, _: context::Context, ctx: InvocationContext) -> AttachResponse {
        trace!(
            session_id = %self.state.session.id,
            env_vars = ctx.process.env.len(),
            has_cwd = ctx.process.cwd.is_some(),
            "received attach context"
        );
        let mut attached = self.state.client_attached.lock();
        if *attached {
            AttachResponse::Busy
        } else {
            *self.state.last_context.lock() = Some(ctx);
            *attached = true;
            AttachResponse::Attached {
                session_id: SessionId(self.state.session.id.clone()),
            }
        }
    }

    async fn detach(self, _: context::Context) {
        let mut attached = self.state.client_attached.lock();
        *attached = false;
    }

    async fn prompt(
        self,
        _: context::Context,
        prompt_id: PromptId,
        text: String,
    ) -> std::result::Result<(), String> {
        let stream = self
            .state
            .events
            .take(prompt_id)
            .await
            .ok_or_else(|| format!("no events socket registered for prompt {prompt_id}"))?;

        let state = self.state.clone();
        tokio::spawn(async move {
            run_prompt(state, stream, text).await;
        });
        Ok(())
    }
}

pub(super) async fn serve_client(
    path: PathBuf,
    state: Arc<ServerState>,
) -> std::result::Result<(), ServerError> {
    let mut listener = tarpc::serde_transport::unix::listen(&path, Bincode::default)
        .await
        .map_err(ServerError::ClientListen)?;
    listener.config_mut().max_frame_length(usize::MAX);
    while let Some(transport) = listener.next().await {
        let transport = match transport {
            Ok(t) => t,
            Err(error) => {
                warn!(%error, "client accept error");
                continue;
            }
        };
        let server = ClientServer {
            state: state.clone(),
        };
        let channel = tarpc::server::BaseChannel::with_defaults(transport);
        tokio::spawn(
            channel
                .execute(server.serve())
                .for_each(|response| async move {
                    tokio::spawn(response);
                }),
        );
    }
    Ok(())
}
