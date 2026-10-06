//! Forge-independent model: forges, access levels, bridge API descriptors.
//!
//! Capability traits live with their planners so each capability reads in one
//! place: [`crate::access`] (`RoleMap`, `Grants`), [`crate::collect`]
//! (`Transport`, `EvidenceSource`), [`crate::status`] (`Transport`,
//! `StatusTarget`), [`crate::profile`] (`ProfileConvention`). Adapters
//! (`cfgj`, `cglb`, `cbkt`, `cghb`) implement them; the CLI wires each forge
//! to its adapter. Nothing here touches the network, clock or filesystem.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

/// Forges understood by identity maps, planners and adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Forge {
    Github,
    Forgejo,
    Gitlab,
    Bitbucket,
}

impl Forge {
    pub fn parse(name: &str) -> crate::Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "github" => Ok(Self::Github),
            "forgejo" => Ok(Self::Forgejo),
            "gitlab" => Ok(Self::Gitlab),
            "bitbucket" => Ok(Self::Bitbucket),
            _ => Err(crate::failure(format!("Unsupported forge: {name}"))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Forgejo => "forgejo",
            Self::Gitlab => "gitlab",
            Self::Bitbucket => "bitbucket",
        }
    }
}

/// Canonical access levels. Forge-native roles normalise to one of these
/// before the merge; anything else fails the run closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Read,
    Write,
    Admin,
}
