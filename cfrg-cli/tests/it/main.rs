//! Integration tests for the `cfrg` binary: one test target instead of eleven.
//!
//! Each former `tests/*.rs` is a module here, so the whole suite links the
//! dependency closure once instead of once per file (build-efficiency rule 1).

mod collect_dialects;
mod forge_clone;
mod forge_sync;
mod gate_create;
mod gate_delete;
mod gate_dispatch;
mod gate_flush;
mod pr_bridge_cli;
mod relay_cli;
mod resolve_lookup;
mod resolve_probes;
mod resolve_routing;

#[path = "support/forge_sync.rs"]
mod support;
