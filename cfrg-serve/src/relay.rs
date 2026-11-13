//! The relay in-process: after a verified event the service runs the relay itself (Bridge design 3.1), through the
//! cfrg relay library API (`cfrg::relay`, the engine on the `forge_bridge` database). There is no queue of its own
//! (R56: the lease in the database is the only coordination), no Forgejo workflow and no hop to another machine.
//!
//! [`Relay`] is the seam: the service holds an `Option<Arc<dyn Relay>>`. Without one (the provider keys and the
//! database credential are not mounted yet) every relay route answers 503 after its signature check, cleanly.
use cfrg::relay::Hint;
use std::{future::Future, pin::Pin};

pub type RelayFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

pub trait Relay: Send + Sync {
    /// Apply one hint (RD1: forge, repository id, kind, number, action; everything else is read from the forges).
    fn apply(&self, hint: Hint) -> RelayFuture<'_>;
}
