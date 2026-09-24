use crate::io::HarnessIo;
use frances_edit::{AnchorStore, EditSession};
use std::{collections::HashMap, ffi::OsString, path::PathBuf, sync::Arc};

pub trait HarnessDeps: HarnessIo {
    type EditorFactory: EditorFactory;
    fn editor_factory(&self) -> &Self::EditorFactory;
    fn current_env(&self) -> Arc<HashMap<OsString, OsString>>;
    fn current_cwd(&self) -> Option<PathBuf>;
    fn editable_roots(&self) -> &[PathBuf];
}

pub trait EditorFactory: Clone + Send + Sync + 'static {
    type Store: AnchorStore + Send + Sync + 'static;
    fn new_session(&self) -> EditSession<Self::Store>;
}

pub type EditorSession<D> = Arc<
    tokio::sync::Mutex<EditSession<<<D as HarnessDeps>::EditorFactory as EditorFactory>::Store>>,
>;
