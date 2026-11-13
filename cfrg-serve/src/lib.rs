//! `cfrg-serve`: the receiver and the release upload of the cfrg bridge (Bridge design, B5).
//!
//! * [`server`]: per-forge signature verification ([`verify`]), replay protection by delivery id, a fast 2xx, then a
//!   Forgejo `workflow_dispatch` of `relay.yml` ([`dispatch`]) carrying only the five fields of [`event::Dispatch`]
//! * [`release`]: the release upload (R45), ported from the Python bridge
//!
//! Events only: there is no timer and no polling (R30). One attempt per event; the next event completes the rest.
#![forbid(unsafe_code)]

pub mod config;
pub mod dispatch;
pub mod event;
pub mod http;
pub mod release;
pub mod server;
pub mod verify;
