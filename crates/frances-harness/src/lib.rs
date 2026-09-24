//! Native platform tools. The host owns model calls and context lifetime;
//! tools own execution over the worker I/O boundary.
pub mod deps;
pub mod instructions;
pub mod io;
pub mod permission;
pub mod tools;

pub use deps::{EditorFactory, HarnessDeps};
pub use io::real::{RealFs, RealIo, WorkerIo};
pub use io::{HarnessFs, HarnessIo, HarnessShell, HarnessShellHandle};
pub use permission::{PermissionRequest, PermissionResponse, PermissionResponseWire};
pub use tools::{ToolError, Tools};

use frances_models_ui::SectionKind;
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug)]
pub enum Output {
    Section(SectionKind),
    Snapshot {
        id: Uuid,
        kind: String,
        snapshot: Value,
    },
    Append {
        id: Uuid,
        payload: Value,
    },
    Settle {
        id: Uuid,
        snapshot: Value,
        artifacts: Vec<(String, Value)>,
    },
    Permission(PermissionRequest),
}

#[derive(Clone)]
pub struct Outputs(pub mpsc::UnboundedSender<Output>);

impl Outputs {
    pub fn send(&self, output: Output) {
        if let Err(error) = self.0.send(output) {
            tracing::debug!(%error, "harness output receiver closed");
        }
    }
    pub fn open(&self, kind: &str, snapshot: Value) -> Uuid {
        let id = Uuid::new_v4();
        self.send(Output::Snapshot {
            id,
            kind: kind.into(),
            snapshot,
        });
        self.send(Output::Section(SectionKind::EntityRef { entity_id: id }));
        id
    }
    pub fn settle(&self, id: Uuid, snapshot: Value) {
        self.send(Output::Settle {
            id,
            snapshot,
            artifacts: vec![],
        });
    }
}
