//! Bitbucket adapter for cfrg: roles, grants, status, profile.
//!
//! Every Bitbucket-specific rule lives here; core planners stay
//! forge-independent and receive these implementations from the CLI.
//!
//! Bitbucket keeps the profile in the workspace description rather than a
//! repository, so there is no profile repository, README path or default
//! directory to project into. Repository names are lowercase and bounded.
//!
//! Rate-limit signals: Bitbucket answers throttled API use with HTTP 429.
//! The core paced executor stops a run on the first 401/403/429/402 and
//! records the halt; the status transport cools down on 429 before retrying
//! later.

#![forbid(unsafe_code)]

pub mod native;

use cfrg::{
    access::{Call, Grants, RoleMap},
    model::{Forge, Level},
    profile::ProfileConvention,
    status::{AuthScheme, State, StatusTarget},
    Result,
};
use serde_json::{json, Value};

/// Bitbucket role vocabulary.
pub struct BitbucketRoles;
/// Bitbucket grant-write API shapes.
pub struct BitbucketGrants;
/// Bitbucket native status API.
pub struct BitbucketStatus;
/// Bitbucket profile conventions.
pub struct BitbucketProfile;

pub static ROLES: BitbucketRoles = BitbucketRoles;
pub static GRANTS: BitbucketGrants = BitbucketGrants;
pub static STATUS: BitbucketStatus = BitbucketStatus;
pub static PROFILE: BitbucketProfile = BitbucketProfile;

impl RoleMap for BitbucketRoles {
    fn normalize_role(&self, role: &str) -> Result<Level> {
        match role.trim().to_ascii_lowercase().as_str() {
            "viewer" | "read" => Ok(Level::Read),
            "member" | "developer" | "write" => Ok(Level::Write),
            "admin" | "owner" => Ok(Level::Admin),
            _ => Err(format!(
                "Unknown bitbucket role; record a documented role instead of guessing: {role}"
            )
            .into()),
        }
    }
}

impl Grants for BitbucketGrants {
    fn write_call(
        &self,
        team: Option<&str>,
        repo: Option<&str>,
        level: Option<Level>,
        org: &str,
        handle: &str,
        _is_new: bool,
    ) -> Call {
        // Endpoint templates are UNVERIFIED (see docs/access-sync.md).
        if level.is_none() {
            let path = if team.is_some() {
                format!("workspaces/{org}/members/{handle}")
            } else {
                format!(
                    "repositories/{org}/{}/permissions-config/users/{handle}",
                    repo.unwrap_or_default()
                )
            };
            return Call {
                forge: Forge::Bitbucket.as_str().into(),
                operation: "revoke".into(),
                method: "DELETE".into(),
                path,
                body: String::new(),
                lookups: Vec::new(),
                endpoint_verified: false,
            };
        }
        let level = level.unwrap_or(Level::Read);
        let (path, body, lookups) = if team.is_some() {
            (
                format!("workspaces/{org}/members/{handle}"),
                format!(
                    "{{\"permission\":\"{}\"}}",
                    match level {
                        Level::Read => "viewer",
                        Level::Write => "member",
                        Level::Admin => "admin",
                    }
                ),
                vec![format!("account id for {handle}")],
            )
        } else {
            (
                format!(
                    "repositories/{org}/{}/permissions-config/users/{handle}",
                    repo.unwrap_or_default()
                ),
                format!(
                    "{{\"permission\":\"{}\"}}",
                    match level {
                        Level::Read => "read",
                        Level::Write => "write",
                        Level::Admin => "admin",
                    }
                ),
                vec![format!("account id for {handle}")],
            )
        };
        Call {
            forge: Forge::Bitbucket.as_str().into(),
            operation: "set-level".into(),
            method: "PUT".into(),
            path,
            body,
            lookups,
            endpoint_verified: false,
        }
    }
}

impl StatusTarget for BitbucketStatus {
    fn validate(&self, origin: &str, repository: &str) -> Result<()> {
        let parts: Vec<_> = repository.split('/').collect();
        if parts.len() != 2
            || parts.iter().any(|p| {
                p.is_empty()
                    || *p == "."
                    || *p == ".."
                    || !p
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            })
        {
            return Err("Invalid Bitbucket repository; want workspace/project"
                .to_owned()
                .into());
        }
        // Cloud build statuses are the only supported Bitbucket API.
        if origin != "https://api.bitbucket.org" {
            return Err("Bitbucket requires its Cloud API origin".to_owned().into());
        }
        Ok(())
    }

    fn auth(&self) -> AuthScheme {
        AuthScheme::Bearer
    }

    fn commit_path(&self, repository: &str, sha: &str) -> String {
        format!("/2.0/repositories/{repository}/commit/{sha}")
    }

    fn status_path(&self, repository: &str, sha: &str) -> String {
        format!("/2.0/repositories/{repository}/commit/{sha}/statuses/build")
    }

    fn status_body(&self, name: &str, url: &str, state: State) -> Value {
        json!({"state":match state {State::Pending => "INPROGRESS", State::Success => "SUCCESSFUL", State::Failure => "FAILED"},"key":status_key(name),"name":name,"url":url})
    }

    fn commit_field(&self) -> &'static str {
        "hash"
    }
}

/// Deterministic build key for a check name. The `ccid-` prefix predates
/// the split and is preserved byte-for-byte so previously posted statuses
/// keep their identity instead of duplicating.
fn status_key(name: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("ccid-{:x}", Sha256::digest(name.as_bytes()))[..40].to_owned()
}

impl ProfileConvention for BitbucketProfile {
    fn profile_repo(&self) -> Option<&'static str> {
        None
    }

    fn readme_path(&self) -> Option<&'static str> {
        None
    }

    fn default_dir(&self) -> Option<&'static str> {
        None
    }

    fn frozen_readonly(&self) -> bool {
        false
    }

    fn valid_repo_name(&self, name: &str) -> bool {
        cfrg::profile::plain_name(name) && name.len() <= 100 && name == name.to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_follow_the_bitbucket_table() {
        assert_eq!(
            BitbucketRoles.normalize_role("viewer").unwrap(),
            Level::Read
        );
        assert_eq!(
            BitbucketRoles.normalize_role("member").unwrap(),
            Level::Write
        );
        assert_eq!(
            BitbucketRoles.normalize_role("developer").unwrap(),
            Level::Write
        );
        assert_eq!(
            BitbucketRoles.normalize_role("ADMIN").unwrap(),
            Level::Admin
        );
        assert!(BitbucketRoles.normalize_role("billing").is_err());
    }

    #[test]
    fn workspace_members_and_repo_permissions_shape_calls() {
        let member = BitbucketGrants.write_call(
            Some("team"),
            None,
            Some(Level::Read),
            "workspace",
            "bob-bb",
            true,
        );
        assert_eq!(member.path, "workspaces/workspace/members/bob-bb");
        assert!(member.body.contains("viewer"));
        let permission = BitbucketGrants.write_call(
            None,
            Some("project"),
            Some(Level::Write),
            "workspace",
            "bob-bb",
            false,
        );
        assert!(permission
            .path
            .ends_with("/permissions-config/users/bob-bb"));
        assert!(permission.body.contains("write"));
    }

    #[test]
    fn status_paths_use_the_cloud_build_api() {
        assert_eq!(
            BitbucketStatus.commit_path("workspace/project", "a"),
            "/2.0/repositories/workspace/project/commit/a"
        );
        assert_eq!(BitbucketStatus.commit_field(), "hash");
        assert!(BitbucketStatus
            .validate("https://api.bitbucket.org", "workspace/project")
            .is_ok());
        assert!(BitbucketStatus
            .validate("https://bitbucket.example", "workspace/project")
            .is_err());
    }

    #[test]
    fn profile_uses_the_workspace_description_with_lowercase_names() {
        assert_eq!(BitbucketProfile.profile_repo(), None);
        assert_eq!(BitbucketProfile.readme_path(), None);
        assert_eq!(BitbucketProfile.default_dir(), None);
        assert!(BitbucketProfile.valid_repo_name("workspace/project"));
        assert!(!BitbucketProfile.valid_repo_name("Workspace/Project"));
        assert!(!BitbucketProfile.valid_repo_name(""));
    }
}

#[cfg(test)]
mod status_body_tests {
    use super::*;

    #[test]
    fn bodies_use_build_states_with_stable_hashed_key() {
        for (state, word) in [
            (State::Pending, "INPROGRESS"),
            (State::Success, "SUCCESSFUL"),
            (State::Failure, "FAILED"),
        ] {
            let body = BitbucketStatus.status_body("ccid/verify", "https://ci.example/1", state);
            assert_eq!(body["state"], word);
            assert_eq!(body["name"], "ccid/verify");
            assert_eq!(body["url"], "https://ci.example/1");
            let key = body["key"].as_str().unwrap();
            assert!(key.starts_with("ccid-"), "{key}");
            assert_eq!(key.len(), 40);
            assert!(key[5..].bytes().all(|b| b.is_ascii_hexdigit()));
        }
        // Pinned vector: "ccid-" plus sha256("verify") hex, cut to 40
        // bytes exactly like the original reporter.
        let body = BitbucketStatus.status_body("verify", "https://ci.example/1", State::Success);
        assert_eq!(body["key"], "ccid-a12dd3a7fd3203a452eb34d91a9be20569d");
        // Same check, same key; different checks differ.
        let again = BitbucketStatus.status_body("verify", "https://ci.example/1", State::Failure);
        let other = BitbucketStatus.status_body("other", "https://ci.example/1", State::Success);
        assert_eq!(again["key"], body["key"]);
        assert_ne!(other["key"], body["key"]);
    }
}
