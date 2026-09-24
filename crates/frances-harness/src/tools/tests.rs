use crate::{EditorFactory, HarnessDeps, HarnessIo, Output, Outputs, RealIo, Tools};
use frances_edit::{EditEngine, EditSession, FakeStore};
use frances_models_llm::ToolCall;
use serde_json::{Value, json};
use std::{collections::HashMap, ffi::OsString, path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Deps<Io: HarnessIo = RealIo> {
    root: Vec<PathBuf>,
    factory: Factory,
    io: Io,
}
#[derive(Clone)]
struct Factory(Arc<EditEngine<FakeStore>>);
impl EditorFactory for Factory {
    type Store = FakeStore;
    fn new_session(&self) -> EditSession<FakeStore> {
        EditSession::new(self.0.clone())
    }
}
impl<Io: HarnessIo> HarnessIo for Deps<Io> {
    type Fs = Io::Fs;
    type Shell = Io::Shell;
    fn fs(&self) -> &Self::Fs {
        self.io.fs()
    }
    fn shell(&self) -> &Self::Shell {
        self.io.shell()
    }
}
impl<Io: HarnessIo> HarnessDeps for Deps<Io> {
    type EditorFactory = Factory;
    fn editor_factory(&self) -> &Factory {
        &self.factory
    }
    fn current_cwd(&self) -> Option<PathBuf> {
        Some(self.root[0].clone())
    }
    fn current_env(&self) -> Arc<HashMap<OsString, OsString>> {
        Arc::new(HashMap::new())
    }
    fn editable_roots(&self) -> &[PathBuf] {
        &self.root
    }
}
fn setup() -> (
    tempfile::TempDir,
    Deps,
    Tools<Deps>,
    mpsc::UnboundedReceiver<Output>,
) {
    let dir = tempfile::tempdir().unwrap();
    let deps = Deps {
        root: vec![dir.path().canonicalize().unwrap()],
        factory: Factory(Arc::new(EditEngine::new(FakeStore::default()))),
        io: RealIo::default(),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let tools = Tools::new(deps.clone(), Outputs(tx));
    (dir, deps, tools, rx)
}
async fn execute<Io: HarnessIo>(
    tools: &mut Tools<Deps<Io>>,
    name: &str,
    args: Value,
) -> Result<String, super::ToolError> {
    tools
        .execute(
            &ToolCall {
                id: "test".into(),
                name: name.into(),
                arguments: args,
                error: None,
            },
            &CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn new_context_requires_read_and_keeps_shared_anchors() {
    let (dir, deps, mut tools, _rx) = setup();
    std::fs::write(dir.path().join("file"), "original\n").unwrap();
    let rendered = execute(&mut tools, "file_read", json!({"path":"file"}))
        .await
        .unwrap();
    let (tx, _rx2) = mpsc::unbounded_channel();
    let mut fresh = Tools::new(deps, Outputs(tx));
    assert!(
        execute(
            &mut fresh,
            "file_overwrite",
            json!({"path":"file","text":"changed"})
        )
        .await
        .is_err()
    );
    let again = execute(&mut fresh, "file_read", json!({"path":"file"}))
        .await
        .unwrap();
    assert_eq!(rendered, again);
    execute(&mut fresh, "file_replace_lines", json!({"path":"file","anchor":rendered.trim_end(),"end_anchor":rendered.trim_end(),"text":"changed"})).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("file")).unwrap(),
        "changed\n"
    );
}

#[tokio::test]
async fn edits_cannot_escape_through_symlinks_or_clobber_existing_files() {
    let (dir, _deps, mut tools, _rx) = setup();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("file"), "outside\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
    let read = execute(&mut tools, "file_read", json!({"path":"link/file"}))
        .await
        .unwrap();
    assert_eq!(read, "1:outside\n");
    for path in ["link/file", "link/new"] {
        assert!(
            execute(&mut tools, "file_new", json!({"path":path,"text":"bad"}))
                .await
                .is_err()
        );
    }
    assert_eq!(
        std::fs::read_to_string(outside.path().join("file")).unwrap(),
        "outside\n"
    );
    execute(
        &mut tools,
        "file_new",
        json!({"path":"new/child","text":"first"}),
    )
    .await
    .unwrap();
    assert!(
        execute(
            &mut tools,
            "file_new",
            json!({"path":"new/child","text":"second"})
        )
        .await
        .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("new/child")).unwrap(),
        "first\n"
    );
}

#[tokio::test]
async fn search_and_variable_filters_round_trip_without_javascript() {
    let (dir, _deps, mut tools, _rx) = setup();
    std::fs::write(dir.path().join("file.txt"), "needle\n").unwrap();
    execute(
        &mut tools,
        "file_find_or_grep",
        json!({"search":"needle","into":"found"}),
    )
    .await
    .unwrap();
    assert_eq!(
        execute(
            &mut tools,
            "var_get",
            json!({"name":"found","filter":".entries | length"})
        )
        .await
        .unwrap(),
        "1"
    );
    execute(
        &mut tools,
        "var_set",
        json!({"name":"text","value":"new content"}),
    )
    .await
    .unwrap();
    execute(
        &mut tools,
        "var_edit",
        json!({"name":"text","filter":". + \"!\""}),
    )
    .await
    .unwrap();
    execute(
        &mut tools,
        "file_new",
        json!({"path":"generated","from":"text"}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("generated")).unwrap(),
        "new content!\n"
    );
    assert!(
        execute(
            &mut tools,
            "var_edit",
            json!({"name":"text","filter":"1,2"})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn invalid_shell_variable_names_never_execute() {
    let (dir, _deps, mut tools, _rx) = setup();
    execute(
        &mut tools,
        "var_set",
        json!({"name":"value","value":"hello"}),
    )
    .await
    .unwrap();
    assert!(
        execute(
            &mut tools,
            "shell_set",
            json!({"name":"X; touch injected; #","from":"value"})
        )
        .await
        .is_err()
    );
    assert!(!dir.path().join("injected").exists());
}

#[tokio::test]
async fn worker_backed_tools_create_read_and_search() {
    let (dir, local, _tools, _rx) = setup();
    let (client_io, worker_io) = tokio::io::duplex(16 * 1024);
    let worker = tokio::spawn(frances_worker::serve(worker_io));
    let client = frances_worker::Client::connect(client_io).await.unwrap();
    let deps = Deps {
        root: local.root,
        factory: local.factory,
        io: crate::WorkerIo::new(client.clone()),
    };
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut tools = Tools::new(deps, Outputs(tx));
    execute(
        &mut tools,
        "file_new",
        json!({"path":"nested/file","text":"hello"}),
    )
    .await
    .unwrap();
    let text = execute(&mut tools, "file_read", json!({"path":"nested/file"}))
        .await
        .unwrap();
    assert!(text.contains("§hello"));
    let search = execute(&mut tools, "file_find_or_grep", json!({"search":"hello"}))
        .await
        .unwrap();
    assert!(search.contains("nested/file"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("nested/file")).unwrap(),
        "hello\n"
    );
    client.shutdown().await.unwrap();
    worker.await.unwrap().unwrap();
}

#[test]
fn variable_schema_accepts_arbitrary_json_without_enabling_strict_mode() {
    let definitions = super::definitions::definitions();
    let schema = definitions
        .iter()
        .find_map(|frances_models_llm::ToolDef::Function(tool)| {
            (tool.name == "var_set").then_some(&tool.parameters)
        })
        .unwrap();
    assert!(!frances_models_llm::tool_args::is_strict_compatible(schema));
    for value in [
        json!(null),
        json!(true),
        json!(42),
        json!("text"),
        json!([1, {"nested": [null]}]),
        json!({"arbitrary": {"keys": 1}}),
    ] {
        assert!(
            frances_models_llm::tool_args::validate(
                &json!({"name": "test", "value": value}),
                schema
            )
            .is_ok()
        );
    }
}
