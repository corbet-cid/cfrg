//! clmr: resolver over DECLARED stores.
//!
//! Pure selection: given a validated [`Request`] (canonical base, explicit
//! aliases, repos with ref kinds and primary identities, ordered stores) and
//! a [`Prober`] answering presence, [`select`] picks at most one store per
//! repo. Pinned hashes take the first verifying store; moving refs only the
//! store whose identity is the proven primary; anything else is a canonical
//! pointer. No network, clock or filesystem access here; the CLI implements
//! [`Prober`] with bounded transports.
//!
//! Identities are opaque validated IDs linked to cfrg placement Policy
//! (`ci[repo.ci].forge`); no forge topology or operator hosts live in this
//! crate. Store order and endpoints are per-runner config data.

#![forbid(unsafe_code)]

mod render;
mod select;
mod validate;

pub use render::{credentials as render_credentials, git_config as render_git_config};
pub use select::{select, Outcome, Prober, RepoDecision, Response, Routing};
pub use validate::{
    canonical_url, normalize_origin, normalize_url, valid_username, Alias, CredentialScope,
    PrimarySource, RefKind, Repository, Request, Store, StoreKind,
};

/// Crate-level error; invalid requests fail closed, probe issues fall back.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub(crate) fn failure(message: impl Into<String>) -> Error {
    std::io::Error::other(message.into()).into()
}

/// README for the workspace index.
pub const _README: &str = include_str!("../README.md");
