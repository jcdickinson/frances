use std::sync::{Arc, Mutex};

use anyhow::Result;
use frances_models_ui::{SectionId, SectionKind};
use frances_session::context::InvocationContext;
use frances_session::events::{
    PermissionRequest, PermissionResponse, PermissionResponseWire, ScrollbackFrame, StreamFrame,
    SurfaceCmd,
};
use frances_session::llm::Usage;
use frances_session::runtime::{SessionRuntime, StartOverrides, install_logging};
use frances_session::session::{Paths, Session};
use frances_session::store;
use frances_session::workspace::Workspace;
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
    title: Option<String>,
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
    Usage {
        usage: Usage,
    },
    Surface {
        command: SurfaceCmd,
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
            respond_permission,
            save_workspace
        ])
        .events(tauri_specta::collect_events![UiEvent])
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
    let events = state
        .events
        .lock()
        .map_err(|_| "event receiver lock poisoned".to_string())?
        .take();

    if let Some(events) = events {
        let app_handle = app.clone();
        tauri::async_runtime::spawn(forward_events(app_handle, events));
        state.runtime.replay_initial_scrollback().await;
    }

    Ok(AppInfo {
        session_id: state.runtime.session.id.to_string(),
        title: state.runtime.session.meta.title.clone(),
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
        .map_err(|_| "permission lock poisoned".to_string())?
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

        if let UiEvent::Surface {
            command: SurfaceCmd::SetTitle { title },
        } = &event
            && let Some(window) = app.get_webview_window("main")
        {
            let _ = window.set_title(title.as_deref().unwrap_or("frances"));
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
        StreamFrame::Usage(usage) => Some(UiEvent::Usage { usage }),
        StreamFrame::Surface(command) => Some(UiEvent::Surface { command }),
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
    let mut pending = match state.permission.lock() {
        Ok(pending) => pending,
        Err(_) => {
            warn!("permission lock poisoned");
            return Some(UiEvent::Error {
                message: "permission state unavailable".to_string(),
            });
        }
    };

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
