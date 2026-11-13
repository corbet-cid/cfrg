//! The ids of the GitHub and GitLab copies, resolved once from their paths and kept (Bridge build B3b).
//!
//! The placement knows a copy's immutable repository id only when something recorded it; the relay addresses
//! repositories by id (RF14), so it resolves a missing id from the copy's path and stores it in the relay database
//! (`relay_copy_ids`). The row is keyed by forge and path, so a changed path is a miss and resolves again; a
//! definitive 404 on a repository drops its row so the next run resolves it again. Nothing else re-resolves.

use crate::{model::Forge, Result};
use std::collections::BTreeMap;

pub trait CopyIds {
    /// Every stored id, by forge and path.
    fn load(&mut self) -> Result<BTreeMap<(Forge, String), String>>;
    fn put(&mut self, forge: Forge, path: &str, repo_id: &str, now: u64) -> Result<()>;
    /// Forgets the row(s) of this id, after a definitive 404.
    fn drop_id(&mut self, forge: Forge, repo_id: &str) -> Result<()>;
}

/// In memory, for tests and dry runs.
#[derive(Default)]
pub struct MemoryCopyIds(pub BTreeMap<(Forge, String), String>);

impl CopyIds for MemoryCopyIds {
    fn load(&mut self) -> Result<BTreeMap<(Forge, String), String>> {
        Ok(self.0.clone())
    }
    fn put(&mut self, forge: Forge, path: &str, repo_id: &str, _now: u64) -> Result<()> {
        self.0.insert((forge, path.to_owned()), repo_id.to_owned());
        Ok(())
    }
    fn drop_id(&mut self, forge: Forge, repo_id: &str) -> Result<()> {
        self.0.retain(|(f, _), id| !(*f == forge && id == repo_id));
        Ok(())
    }
}
