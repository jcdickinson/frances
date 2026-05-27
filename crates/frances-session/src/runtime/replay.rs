//! Initial scrollback replay — emitted into the events channel as soon
//! as the TUI is wired up so the user sees the prior workflow's
//! committed blocks before any live frames arrive.

use uuid::Uuid;

use crate::Result;
use crate::events::{ScrollbackFrame, StreamFrame};
use crate::store::Database;

use super::EventsChannel;

/// Push the startup replay burst into `events`. With an active
/// workflow this is the same [`ScrollbackFrame::Reset`] / replay /
/// [`ScrollbackFrame::End`] bracket emitted by
/// [`crate::scrollback::replay_to_channel`]; with no active workflow
/// we still emit an empty bracket so the TUI clears any stale
/// in-memory scrollback before going live.
pub(super) async fn write_initial_replay(
    events: &EventsChannel,
    db: &Database,
    active_instance: Option<Uuid>,
) -> Result<()> {
    match active_instance {
        Some(instance) => crate::scrollback::replay_to_channel(events, db, instance).await,
        None => {
            events.send(StreamFrame::Scrollback(ScrollbackFrame::Reset {
                instance_id: Uuid::nil(),
            }));
            events.send(StreamFrame::Scrollback(ScrollbackFrame::End));
            Ok(())
        }
    }
}
