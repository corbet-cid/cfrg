//! cfrg: forge placement, reconciliation, status, bridge and evidence collection.
//!
//! The forge-independent model lives in [`model`]: forges, access levels and
//! bridge descriptors. Capability traits live with their planners so each
//! capability reads in one place — [`access`] (`RoleMap`, `Grants`),
//! [`collect`] (`Transport`, `EvidenceSource`), [`status`] (`Transport`,
//! `StatusTarget`), [`profile`] (`ProfileConvention`) — and are implemented
//! once per adapter (`cfgj`, `cglb`, `cbkt`, `cghb`), which hold every
//! forge-specific rule. The CLI wires each forge to its adapter.
//!
//! Pure planners ([`placement`], [`access`], [`profile`], the [`sync`] ref
//! table) never touch the network, clock or filesystem. Bounded transports
//! ([`process`], Git, curl) and the paced [`access::apply_plan`] executor
//! (stops on the first 401/403/429/402, records state) carry the side
//! effects. The binary maps these modules to
//! `cfrg validate/plan/decide/clone/sync/status/bridge/collect/access`.

#![forbid(unsafe_code)]

use std::{collections::BTreeMap, ffi::OsString, io, sync::atomic::AtomicBool};

pub const SOURCE_REVISION: &str = env!("CFRG_SOURCE_REVISION");
/// Target triple this binary was compiled for.
pub const TARGET: &str = env!("CFRG_TARGET");
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub type Environment = BTreeMap<OsString, OsString>;

pub mod access;
pub mod bridge;
pub mod collect;
pub mod land;
pub mod model;
pub mod native;
pub mod placement;
pub mod process;
pub mod profile;
pub mod release;
pub mod serve;
pub mod status;
pub mod sync;

pub(crate) fn failure(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::other(message.into()).into()
}

fn event(value: serde_json::Value) {
    println!("{value}");
}

fn validate_command(argv: &[String]) -> Result<()> {
    if argv.first().is_none_or(String::is_empty) || argv.iter().any(|arg| arg.contains('\0')) {
        return Err(failure(
            "Commands require a nonempty executable and arguments without NUL bytes",
        ));
    }
    Ok(())
}

/// Canonical `host/owner/repo` identity for one forge URL, without credentials.
pub(crate) fn canonical_repository(url: &str) -> Result<String> {
    let location = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        rest.to_owned()
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        rest.rsplit_once('@')
            .map_or(rest, |(_, path)| path)
            .to_owned()
    } else if let Some((host, path)) = url.strip_prefix("git@").and_then(|v| v.split_once(':')) {
        format!("{host}/{path}")
    } else {
        return Err(failure(
            "Repository identity requires an HTTP(S) or SSH forge URL",
        ));
    };
    let location = location.trim_end_matches('/').trim_end_matches(".git");
    let parts: Vec<_> = location.split('/').collect();
    if parts.len() < 3
        || parts.iter().any(|part| {
            part.is_empty()
                || *part == "."
                || *part == ".."
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-:".contains(&b))
        })
    {
        return Err(failure("Repository identity must contain a plain forge host and repository path, without credentials"));
    }
    Ok(format!(
        "{}/{}",
        parts[0].to_ascii_lowercase(),
        parts[1..].join("/")
    ))
}

#[cfg(all(windows, feature = "windows-experimental"))]
mod windows;
