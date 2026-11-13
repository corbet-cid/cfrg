//! `cfrg relay`: the issue relay core (Bridge design §3.1, §3.2, §5; operating model R37, R38, R41, R50, R51).
//!
//! | ID | Where |
//! |---|---|
//! | R37, RD2 hub and spoke | [`pass`]: every change is written to the Forgejo hub first, then fanned out |
//! | R38 issues everywhere, R50 what is relayed | [`pass`], [`merge`], [`cascade`] |
//! | R41 relay identity and credit, RD9 markers | [`marker`] (format and trust), [`pass`] (relay writes only) |
//! | R51 abuse gate | [`pass`] (screen and the per-author cap), [`forge::Screen`] |
//! | R9, RD11, RF9 caps, holds, kill switch | [`pacing`], [`engine`] |
//! | R10, R11, RD12 contributed projects | [`crate::contributed::Contributed::refuse`], called before any read or write |
//! | RD1 events are hints, RD3 merge, RD4 loop guards | [`engine`] (apply), [`merge`], [`pass`] |
//! | §5 mapping, lease, intent, tombstone | [`store::Store`], [`pg::PgStore`] |
//!
//! The relay reaches a forge only through [`forge::RouterForge`] (the drivers implement it) and keeps its state in a
//! [`store::Store`]: PostgreSQL `forge_bridge` in production, [`store::MemStore`] in tests. There is no timer, no
//! catch-up and no rebuild of its own: Forgejo Actions runs `apply` per event and `sweep` once a day (R30, R49).

pub mod cascade;
pub mod engine;
pub mod forge;
pub mod marker;
pub mod merge;
pub mod model;
pub mod ops;
pub mod pacing;
pub mod pass;
pub mod pg;
pub mod pgtls;
pub mod store;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_ops;
#[cfg(test)]
mod tests_pg;
#[cfg(test)]
mod tests_tls;

pub use engine::{Engine, Outcome, Report};
pub use forge::{Forges, HoldAll, RouterForge, Screen, ScreenVerdict};
pub use model::{Hint, HintAction, RouterConfig};
