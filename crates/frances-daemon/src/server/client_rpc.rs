use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use tarpc::context;
use tarpc::server::Channel;
use tarpc::tokio_serde::formats::Bincode;
use tracing::{trace, warn};

use crate::context::InvocationContext;
use crate::protocol::{ApprovalChoice, ApprovalId, AttachResponse, Client, SessionId};

use super::ApprovalResponseError;
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
        {
            let mut attached = self.state.client_attached.lock();
            if *attached {
                return AttachResponse::Busy;
            }
            *self.state.last_context.lock() = Some(ctx);
            *attached = true;
        }

        // Wait briefly for the TUI's events socket to be installed
        // (the TUI opens it just before calling attach; in practice
        // it's already here). If it never arrives we still return
        // attached — the client just gets no initial replay.
        if self.state.events.wait_for_stream().await {
            let active_instance = self.state.workflow_stack.active_instance().await;
            let mut guard = self.state.events.lock().await;
            if let Some(stream) = guard.stream() {
                let result =
                    write_initial_replay(stream, self.state.workflow_stack.db(), active_instance)
                        .await;
                if let Err(error) = result {
                    warn!(%error, "initial scrollback replay failed; events stream may be unusable");
                    guard.drop_stream();
                }
            }
        } else {
            warn!("events socket never arrived; attach returning without initial replay");
        }

        AttachResponse::Attached {
            session_id: SessionId(self.state.session.id.clone()),
        }
    }

    async fn detach(self, _: context::Context) {
        let mut attached = self.state.client_attached.lock();
        *attached = false;
    }

    async fn prompt(self, _: context::Context, text: String) -> std::result::Result<(), String> {
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut guard = state.events.lock().await;
            let stream = match guard.stream() {
                Some(s) => s,
                None => {
                    warn!("prompt arrived with no events stream installed; dropping");
                    return;
                }
            };
            run_prompt(state.clone(), stream, text).await;
        });
        Ok(())
    }

    async fn respond_approval(
        self,
        _: context::Context,
        id: ApprovalId,
        choice: ApprovalChoice,
    ) -> std::result::Result<(), String> {
        match self.state.approvals.respond(id, choice) {
            Ok(()) => Ok(()),
            Err(ApprovalResponseError::UnknownId) => {
                Err(format!("no pending approval with id {id}"))
            }
            Err(ApprovalResponseError::Dropped) => Err(format!(
                "approval {id} was dropped before the response landed"
            )),
        }
    }
}

/// Run the attach-time replay path. With an active workflow this is
/// the same `ScrollbackReset` / replay / `ScrollbackReplayEnd` burst
/// emitted by `scrollback::replay_to_stream`; with no active workflow
/// we still emit an empty bracket so the TUI clears any stale
/// in-memory scrollback before going live.
async fn write_initial_replay(
    stream: &mut tokio::net::UnixStream,
    db: &crate::store::Database,
    active_instance: Option<uuid::Uuid>,
) -> crate::Result<()> {
    use crate::protocol::StreamFrame;
    use crate::transport::write_message;

    match active_instance {
        Some(instance) => crate::scrollback::replay_to_stream(stream, db, instance).await,
        None => {
            write_message(
                stream,
                &StreamFrame::ScrollbackReset {
                    instance_id: uuid::Uuid::nil(),
                },
            )
            .await?;
            write_message(stream, &StreamFrame::ScrollbackReplayEnd).await?;
            Ok(())
        }
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
