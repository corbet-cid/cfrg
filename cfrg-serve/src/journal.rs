//! The release intent journal: an upload is recorded before it starts and cleared when it finished, so a restart that cuts
//! one off (any Secret change restarts the pod) leaves the intent open and the next start, or the hourly tick, finishes it.
//! No queue of its own: the journal only remembers which (repository, tag) still owes a run; the upload itself is idempotent.
use std::sync::Arc;

pub trait Journal: Send + Sync {
    fn open(&self, repo: &str, tag: &str) -> Result<(), String>;
    fn clear(&self, repo: &str, tag: &str) -> Result<(), String>;
    fn open_intents(&self) -> Result<Vec<(String, String)>, String>;
}

/// The journal in the bridge database (`forge_bridge`, table release_intents). Each call connects on a blocking thread: releases are rare.
pub struct PgJournal {
    settings: Arc<cfrg_relay::Settings>,
}

impl PgJournal {
    pub fn new(settings: Arc<cfrg_relay::Settings>) -> Self {
        Self { settings }
    }

    fn with<T>(
        &self,
        f: impl FnOnce(&cfrg::relay::pg::PgStore) -> cfrg::Result<T>,
    ) -> Result<T, String> {
        let store = cfrg::relay::pg::PgStore::connect(&self.settings.database)
            .map_err(|e| e.to_string())?;
        f(&store).map_err(|e| e.to_string())
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Journal for PgJournal {
    fn open(&self, repo: &str, tag: &str) -> Result<(), String> {
        self.with(|s| s.release_intent_open(repo, tag, now()))
    }
    fn clear(&self, repo: &str, tag: &str) -> Result<(), String> {
        self.with(|s| s.release_intent_clear(repo, tag))
    }
    fn open_intents(&self) -> Result<Vec<(String, String)>, String> {
        self.with(|s| s.release_intents_open())
    }
}
