//! Declarative native replication control plane. No Git data transfer here.
use crate::{failure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub mod http;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub schema: u32,
    pub source: Endpoint,
    /// How a sender writes into the Forgejo source once it is a receiver.
    #[serde(default)]
    pub receiver: Option<Receiver>,
    /// Base URL of the default primary (the git-pointer view of the placement).
    #[serde(default)]
    pub default: Option<String>,
    /// Lowercase `owner/repo` to the base URL of its primary, exceptions only.
    #[serde(default)]
    pub primaries: BTreeMap<String, String>,
    pub repositories: Vec<Repository>,
}

/// The identity senders write as when Forgejo is a receiver, and the
/// environment reference holding its (narrowly scoped) credential.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Receiver {
    pub mirror_user: String,
    pub password_env: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub origin: String,
    pub token_env: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub path: String,
    pub source_id: u64,
    pub private: bool,
    pub default_branch: String,
    pub content: Content,
    /// Positive content/identity exceptions stay declared, never silently skipped.
    pub hold: Option<String>,
    pub destinations: Vec<Destination>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub provider: Provider,
    pub endpoint: Endpoint,
    pub path: String,
    /// Namespace ID for GitLab; existing project key for Bitbucket.
    pub namespace: String,
    /// Immutable destination identity once known. Required for existing repos.
    pub repository_id: Option<String>,
    pub mirror_user: String,
    pub password_env: String,
    #[serde(default)]
    pub use_ssh: bool,
    pub interval_seconds: u64,
    #[serde(default)]
    pub branch_filter: String,
    pub hold: Option<String>,
    #[serde(default)]
    pub absent: bool,
    /// Explicit ownership claim, also needed for declarative deletion.
    pub remote_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Gitlab,
    Bitbucket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Content {
    NativeGit,
    Lfs,
    Profile,
}

impl Placement {
    /// Read the placement from either view of the one declared file: the
    /// native view itself, or the whole `lib/placement.json` (its `native`
    /// object plus the top-level `default` and `primaries`).
    pub fn from_document(bytes: &[u8]) -> Result<Self> {
        let mut value: serde_json::Value = serde_json::from_slice(bytes)?;
        if let Some(mut native) = value.get_mut("native").map(serde_json::Value::take) {
            for key in ["default", "primaries"] {
                if let (Some(top), Some(object)) = (value.get(key), native.as_object_mut()) {
                    object.entry(key).or_insert_with(|| top.clone());
                }
            }
            value = native;
        }
        Ok(serde_json::from_value(value)?)
    }

    /// Base URL of the primary of `path`: its exception, else the declared
    /// default, else the Forgejo source (every repository before a switch).
    pub fn primary_of(&self, path: &str) -> String {
        let url = self
            .primaries
            .get(&path.to_ascii_lowercase())
            .or(self.default.as_ref())
            .unwrap_or(&self.source.origin);
        url.trim_end_matches('/').to_string()
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != 1 {
            return Err(failure("Unsupported native placement schema"));
        }
        self.source.validate()?;
        for url in self.primaries.values().chain(self.default.iter()) {
            Endpoint {
                origin: url.clone(),
                token_env: "UNUSED".into(),
            }
            .validate()?;
        }
        if let Some(receiver) = &self.receiver {
            env_name(&receiver.password_env)?;
            if receiver.mirror_user.is_empty()
                || receiver.mirror_user.contains('@')
                || receiver.mirror_user.chars().any(char::is_control)
            {
                return Err(failure(
                    "Mirror principal must be an ID/login, never an email",
                ));
            }
        }
        let mut sources = BTreeSet::new();
        let mut targets = BTreeSet::new();
        for repo in &self.repositories {
            path(&repo.path, false)?;
            if repo.source_id == 0 || !sources.insert(&repo.path) || repo.default_branch.is_empty()
            {
                return Err(failure("Missing or duplicate source identity"));
            }
            for dest in &repo.destinations {
                dest.endpoint.validate()?;
                path(&dest.path, dest.provider == Provider::Gitlab)?;
                if !targets.insert((&dest.endpoint.origin, &dest.path))
                    || dest.interval_seconds < 600
                {
                    return Err(failure(
                        "Duplicate destination or interval below ten minutes",
                    ));
                }
                env_name(&dest.password_env)?;
                if dest.use_ssh && dest.provider != Provider::Gitlab {
                    return Err(failure("SSH enrollment requires GitLab deploy keys"));
                }
                if dest.mirror_user.is_empty()
                    || dest.mirror_user.contains('@')
                    || dest.mirror_user.chars().any(char::is_control)
                {
                    return Err(failure(
                        "Mirror principal must be an ID/login, never an email",
                    ));
                }
                if let Some(remote) = &dest.remote_name {
                    component(remote)?;
                }
                if dest.absent && dest.remote_name.is_none() {
                    return Err(failure("Deletion requires declared remote_name ownership"));
                }
            }
        }
        Ok(())
    }
}

impl Endpoint {
    pub fn validate(&self) -> Result<()> {
        env_name(&self.token_env)?;
        let host = self
            .origin
            .strip_prefix("https://")
            .ok_or_else(|| failure("HTTPS origin required"))?;
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
        {
            return Err(failure(
                "Origin must contain only an HTTPS host and optional port",
            ));
        }
        Ok(())
    }
}

pub fn component(value: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(failure("Invalid repository component"));
    }
    Ok(())
}

pub fn path(value: &str, nested: bool) -> Result<()> {
    let parts: Vec<_> = value.split('/').collect();
    if parts.len() < 2 || (!nested && parts.len() != 2) {
        return Err(failure("Invalid repository path"));
    }
    for part in parts {
        component(part)?;
    }
    Ok(())
}

pub fn env_name(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(failure("Invalid credential environment reference"));
    }
    Ok(())
}

pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_views_of_the_one_placement_file_load_and_name_the_primary() {
        let native = r#"{"schema":1,"source":{"origin":"https://forge.example","token_env":"T"},"repositories":[]}"#;
        let whole = r#"{"version":1,"default":"https://forge.example","primaries":{"team/probe":"https://gitlab.example"},
            "native":{"schema":1,"source":{"origin":"https://forge.example","token_env":"T"},"repositories":[]}}"#;
        let plain = Placement::from_document(native.as_bytes()).unwrap();
        assert_eq!(plain.primary_of("any/repo"), "https://forge.example");
        let placement = Placement::from_document(whole.as_bytes()).unwrap();
        placement.validate().unwrap();
        assert_eq!(placement.primary_of("Team/Probe"), "https://gitlab.example");
        assert_eq!(placement.primary_of("team/other"), "https://forge.example");
    }

    #[test]
    fn identities_cannot_change_request_authority() {
        for bad in ["a/../b", "a/x?token=x", "a/%2fsecret", "a/b#c", "a/b\\c"] {
            assert!(path(bad, true).is_err());
        }
        for bad in [
            "https://user@host",
            "https://host/path",
            "http://host",
            "https://host?secret",
        ] {
            assert!(Endpoint {
                origin: bad.into(),
                token_env: "TOKEN".into()
            }
            .validate()
            .is_err());
        }
        assert_eq!(encode("a/b*"), "a%2Fb%2A");
    }
}
