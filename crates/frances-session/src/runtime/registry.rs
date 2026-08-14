//! Published UI entity state.
//!
//! The single publish point for latest-wins UI facts (see
//! [`Entity`]). Holds the current value of every entity and re-emits
//! the whole entity into the events channel on each change, so the UI
//! can mirror state with no read-back commands. Both entities are
//! singletons today; instanced entities (shells, agents) grow a keyed
//! collection here when they land.

use parking_lot::Mutex;

use super::EventsChannel;
use crate::events::{Entity, SessionEntity, StreamFrame, WorkspaceEntity};

pub struct UiRegistry {
    workspace: Mutex<WorkspaceEntity>,
    session: Mutex<SessionEntity>,
    events: EventsChannel,
}

impl UiRegistry {
    pub fn new(events: EventsChannel, workspace: WorkspaceEntity, session: SessionEntity) -> Self {
        Self {
            workspace: Mutex::new(workspace),
            session: Mutex::new(session),
            events,
        }
    }

    /// Queue an upsert for every entity — the attach-time snapshot.
    pub fn publish_all(&self) {
        self.send(Entity::Workspace(self.workspace.lock().clone()));
        self.send(Entity::Session(self.session.lock().clone()));
    }

    pub fn set_workspace(&self, workspace: WorkspaceEntity) {
        *self.workspace.lock() = workspace.clone();
        self.send(Entity::Workspace(workspace));
    }

    pub fn update_session(&self, apply: impl FnOnce(&mut SessionEntity)) {
        let mut session = self.session.lock();
        apply(&mut session);
        let snapshot = session.clone();
        drop(session);
        self.send(Entity::Session(snapshot));
    }

    /// Seeds a booting workflow's `getTitle` (via `WorkflowDeps`).
    pub fn session_title(&self) -> Option<String> {
        self.session.lock().title.clone()
    }

    fn send(&self, entity: Entity) {
        self.events.send(StreamFrame::Entity(entity));
    }
}
