use std::sync::Arc;

use anyhow::Result;
use frances_models_ui::{Lifecycle, SectionId, SectionKind};
use frances_session::context::InvocationContext;
use frances_session::entities::{SESSION_KIND, SessionSnapshot};
use frances_session::events::{
    PermissionRequest, PermissionResponse, PermissionResponseWire, ScrollbackFrame, StreamFrame,
};
use frances_session::runtime::{SessionRuntime, StartOverrides, install_logging};
use frances_session::session::{Paths, Session};
use frances_session::store;
use frances_session::workspace::Workspace;
use parking_lot::Mutex;
use serde::Serialize;
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;
use tauri_specta::Event as _;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

struct Backend {
    runtime: Arc<SessionRuntime>,
    events: Mutex<Option<mpsc::UnboundedReceiver<StreamFrame>>>,
    permission: Mutex<Option<oneshot::Sender<PermissionResponse>>>,
}

#[derive(Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    session_id: String,
}

#[derive(Clone, Serialize, specta::Type, tauri_specta::Event)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UiEvent {
    Reset,
    ReplayEnd,
    SectionAppend {
        id: SectionId,
        kind: SectionKind,
        delta: String,
    },
    SectionClose {
        id: SectionId,
        truncated: bool,
    },
    /// Latest-wins entity state. `snapshot` is opaque at this boundary;
    /// the frontend picks a renderer by `kind` and interprets it there.
    EntityUpsert {
        entity_id: String,
        kind: String,
        lifecycle: Lifecycle,
        snapshot: serde_json::Value,
    },
    /// One item of a subscribed entity's stream; `seq` dedupes across
    /// the catch-up/live splice.
    EntityStream {
        entity_id: String,
        seq: u64,
        payload: serde_json::Value,
    },
    Error {
        message: String,
    },
    Permission {
        prompt: String,
    },
}

pub fn run(workspace: Workspace, workflow: Option<String>) -> Result<()> {
    let paths = Paths::discover()?;
    let session = paths.create_session(&workspace)?;
    let invocation = InvocationContext::capture(workspace);
    install_logging(&session)?;

    let specta = specta_builder();
    #[cfg(debug_assertions)]
    export_bindings(&specta)?;

    let session_for_setup = session.clone();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(specta.invoke_handler())
        .setup(move |app| {
            specta.mount_events(app);

            let (runtime, events) = tauri::async_runtime::block_on(start_runtime(
                session_for_setup.clone(),
                invocation,
                workflow,
            ))?;

            if let Some(title) = &session_for_setup.meta.title
                && let Some(window) = app.get_webview_window("main")
            {
                window.set_title(title)?;
            }

            debug!(session_id = %session_for_setup.id, "starting desktop app");
            app.manage(Backend {
                runtime,
                events: Mutex::new(Some(events)),
                permission: Mutex::new(None),
            });
            Ok(())
        })
        .build(tauri::generate_context!())?;

    app.run(|handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            handle.state::<Backend>().runtime.shutdown();
        }
    });
    Ok(())
}

fn specta_builder() -> tauri_specta::Builder {
    tauri_specta::Builder::<tauri::Wry>::new()
        .commands(tauri_specta::collect_commands![
            frontend_ready,
            send_prompt,
            interrupt,
            toggle_devtools,
            respond_permission,
            save_workspace,
            subscribe_entity,
            unsubscribe_entity,
            read_entity_artifact
        ])
        .events(tauri_specta::collect_events![UiEvent])
        // Snapshot payloads are opaque JSON on the wire; export the
        // Rust-produced singleton shapes so the frontend can cast them.
        .typ::<frances_session::entities::WorkspaceSnapshot>()
        .typ::<SessionSnapshot>()
}

/// Write the generated TypeScript bindings into the frontend source tree.
fn export_bindings(specta: &tauri_specta::Builder) -> Result<()> {
    specta.export(
        specta_typescript::Typescript::default()
            .bigint(specta_typescript::BigIntExportBehavior::Number),
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../frontend/src/bindings.ts"
        ),
    )?;
    Ok(())
}

async fn start_runtime(
    session: Session,
    invocation: InvocationContext,
    workflow: Option<String>,
) -> Result<(Arc<SessionRuntime>, mpsc::UnboundedReceiver<StreamFrame>)> {
    let db = store::open(&session).await?;
    let overrides = StartOverrides {
        default_workflow: workflow,
        ..StartOverrides::default()
    };
    Ok(SessionRuntime::start_with(session, db, invocation, overrides).await?)
}

#[tauri::command]
#[specta::specta]
async fn frontend_ready(
    app: tauri::AppHandle,
    state: tauri::State<'_, Backend>,
) -> Result<AppInfo, String> {
    let events = state.events.lock().take();

    if let Some(events) = events {
        let app_handle = app.clone();
        tauri::async_runtime::spawn(forward_events(app_handle, events));
        state.runtime.replay_initial_scrollback().await;
    }

    Ok(AppInfo {
        session_id: state.runtime.session.id.to_string(),
    })
}

#[tauri::command]
#[specta::specta]
fn send_prompt(state: tauri::State<'_, Backend>, text: String) {
    state.runtime.prompt(text);
}

#[tauri::command]
#[specta::specta]
fn interrupt(state: tauri::State<'_, Backend>) {
    state.runtime.interrupt();
}

/// Ctrl+Shift+I in the frontend. Backed by tauri's `devtools` feature.
#[tauri::command]
#[specta::specta]
fn toggle_devtools(window: tauri::WebviewWindow) {
    if window.is_devtools_open() {
        window.close_devtools();
    } else {
        window.open_devtools();
    }
}

/// Show a save dialog and write the current workspace as a workspace
/// file. Returns the saved path, or `None` if the user cancelled.
#[tauri::command]
#[specta::specta]
async fn save_workspace(
    app: tauri::AppHandle,
    state: tauri::State<'_, Backend>,
) -> Result<Option<String>, String> {
    let workspace = state.runtime.invocation.lock().workspace.clone();

    let (reply, chosen) = oneshot::channel();
    app.dialog()
        .file()
        .set_title("Save Workspace")
        .add_filter("frances workspace", &["toml"])
        .set_directory(workspace.primary_dir())
        .set_file_name("workspace.toml")
        .save_file(move |path| {
            let _ = reply.send(path);
        });

    let Some(path) = chosen
        .await
        .map_err(|_| "save dialog closed without a response".to_string())?
    else {
        return Ok(None);
    };

    let path = path.into_path().map_err(|error| error.to_string())?;
    workspace.save(&path).map_err(|error| error.to_string())?;
    Ok(Some(path.display().to_string()))
}

/// Attach the frontend to an entity's stream. `catch_up` replays every
/// persisted item before tailing (a tab opening); without it the
/// stream just starts tailing (an inline live view).
#[tauri::command]
#[specta::specta]
async fn subscribe_entity(
    state: tauri::State<'_, Backend>,
    entity_id: String,
    catch_up: bool,
) -> Result<(), String> {
    let id = entity_id.parse().map_err(|_| "bad entity id".to_string())?;
    state
        .runtime
        .entities
        .subscribe(id, catch_up)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
#[specta::specta]
fn unsubscribe_entity(state: tauri::State<'_, Backend>, entity_id: String) -> Result<(), String> {
    let id = entity_id.parse().map_err(|_| "bad entity id".to_string())?;
    state.runtime.entities.unsubscribe(id);
    Ok(())
}

/// Point-read one settle artifact (e.g. a shell's `llm_digest`).
#[tauri::command]
#[specta::specta]
async fn read_entity_artifact(
    state: tauri::State<'_, Backend>,
    entity_id: String,
    tag: String,
) -> Result<Option<serde_json::Value>, String> {
    let id = entity_id.parse().map_err(|_| "bad entity id".to_string())?;
    state
        .runtime
        .entities
        .read_artifact(id, &tag)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
#[specta::specta]
fn respond_permission(
    state: tauri::State<'_, Backend>,
    decision: String,
    details: Option<String>,
) -> Result<(), String> {
    let reply = state
        .permission
        .lock()
        .take()
        .ok_or_else(|| "there is no pending permission request".to_string())?;

    let response = match decision.as_str() {
        "yes" => PermissionResponseWire::Yes { details },
        "no" => PermissionResponseWire::No { details },
        "chat" => PermissionResponseWire::RedirectToChat {
            content: details.unwrap_or_default(),
        },
        _ => return Err(format!("unknown permission decision: {decision}")),
    };

    state
        .runtime
        .respond_permission(reply, response)
        .map_err(|error| error.to_string())
}

async fn forward_events(app: tauri::AppHandle, mut events: mpsc::UnboundedReceiver<StreamFrame>) {
    while let Some(frame) = events.recv().await {
        let Some(event) = convert_frame(&app, frame) else {
            continue;
        };

        if let UiEvent::EntityUpsert { kind, snapshot, .. } = &event
            && kind == SESSION_KIND
            && let Ok(session) = serde_json::from_value::<SessionSnapshot>(snapshot.clone())
            && let Some(window) = app.get_webview_window("main")
        {
            let _ = window.set_title(session.title.as_deref().unwrap_or("frances"));
        }

        if let Err(error) = event.emit(&app) {
            warn!(%error, "emit session event failed");
        }
    }
}

fn convert_frame(app: &tauri::AppHandle, frame: StreamFrame) -> Option<UiEvent> {
    match frame {
        StreamFrame::SectionAppend { id, kind, delta } => {
            Some(UiEvent::SectionAppend { id, kind, delta })
        }
        StreamFrame::SectionClose { id } => Some(UiEvent::SectionClose {
            id,
            truncated: false,
        }),
        StreamFrame::SectionTruncated { id } => Some(UiEvent::SectionClose {
            id,
            truncated: true,
        }),
        StreamFrame::EntityUpsert { envelope, snapshot } => Some(UiEvent::EntityUpsert {
            entity_id: envelope.entity_id.to_string(),
            kind: envelope.kind,
            lifecycle: envelope.lifecycle,
            snapshot,
        }),
        StreamFrame::EntityStream {
            entity_id,
            seq,
            payload,
        } => Some(UiEvent::EntityStream {
            entity_id: entity_id.to_string(),
            seq,
            payload,
        }),
        StreamFrame::Error(message) => Some(UiEvent::Error { message }),
        StreamFrame::Permission(request) => store_permission(app, request),
        StreamFrame::Scrollback(frame) => match frame {
            ScrollbackFrame::Reset { .. } => Some(UiEvent::Reset),
            ScrollbackFrame::SectionAppend { id, kind, delta } => {
                Some(UiEvent::SectionAppend { id, kind, delta })
            }
            ScrollbackFrame::SectionClose { id } => Some(UiEvent::SectionClose {
                id,
                truncated: false,
            }),
            ScrollbackFrame::SectionTruncated { id } => Some(UiEvent::SectionClose {
                id,
                truncated: true,
            }),
            ScrollbackFrame::Error(message) => Some(UiEvent::Error { message }),
            ScrollbackFrame::End => Some(UiEvent::ReplayEnd),
        },
    }
}

fn store_permission(app: &tauri::AppHandle, request: PermissionRequest) -> Option<UiEvent> {
    let state = app.state::<Backend>();
    let mut pending = state.permission.lock();

    if pending.is_some() {
        warn!("received a second permission request while one is pending");
        return Some(UiEvent::Error {
            message: "another permission request is already pending".to_string(),
        });
    }

    *pending = Some(request.reply);
    Some(UiEvent::Permission {
        prompt: request.prompt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regenerates `frontend/src/bindings.ts`. The debug desktop launch
    /// does the same; this keeps the bindings reproducible headlessly.
    #[test]
    fn export_typescript_bindings() {
        export_bindings(&specta_builder()).expect("export bindings");
    }
}
