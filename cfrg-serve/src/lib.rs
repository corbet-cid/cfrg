//! `cfrg-serve`: the receiver and the release upload of the cfrg bridge (Bridge design, B5).
//!
//! * [`server`]: per-forge signature verification ([`verify`]), replay protection by delivery id, a fast 2xx, then a
//!   the relay in-process ([`relay`]) on the five fields of [`event::Dispatch`]
//! * [`release`]: the release upload (R45), with a parity test suite
//!
//! Events only: there is no timer and no polling (R30). One attempt per event; the next event completes the rest.
#![forbid(unsafe_code)]

pub mod allow;
pub mod config;
pub mod event;
pub mod hooks;
pub mod http;
pub mod live;
pub mod relay;
pub mod release;
pub mod server;
pub mod verify;
pub mod wiki;
