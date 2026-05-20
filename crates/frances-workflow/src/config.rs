use std::path::PathBuf;

use serde::Deserialize;
use uuid::Uuid;

/// One row of the `workflows` config table.
///
/// Each workflow owns a chunk of the per-session DB schema via the
/// runtime's migration system: `id` is its stable [`Uuid`] entity, and
/// `migrations` lists the SQL files in apply order. Migration paths are
/// resolved **relative to `file`'s parent directory** — co-locate
/// `0001_init.sql` with the script and refer to it as
/// `migrations = ["0001_init.sql"]`.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowConfig {
    pub id: Uuid,
    pub file: PathBuf,
    /// SQL migration files, resolved relative to [`Self::file`]'s
    /// parent. Order is the apply order; once a workflow ships, treat
    /// the prefix as immutable — the migration runner refuses to load
    /// when a recorded migration's name or content drifts.
    #[serde(default)]
    pub migrations: Vec<PathBuf>,
}
