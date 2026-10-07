//! GitHub adapter for cfrg: roles, grants, evidence, profile.
//!
//! Every GitHub-specific rule lives here; core planners stay
//! forge-independent and receive these implementations from the CLI.
//!
//! GitHub is deliberately NOT a status destination: there is no `Status`
//! implementation here, and the core reporter rejects it.
//!
//! Rate-limit signals: GitHub answers throttled API use with HTTP 429 or a
//! 403 carrying exhausted rate-limit headers. The core paced executor stops
//! a run on the first 401/403/429/402 and records the halt.

#![forbid(unsafe_code)]

/// Declared landing and release capability of this adapter. Never guessed:
/// `unsupported` means the procedure is not implemented here.
pub const LAND: cfrg::land::Capability =
    cfrg::land::Capability::unsupported("unsupported: GitHub is frozen, no writes of any kind");
pub const RELEASE: cfrg::land::Capability =
    cfrg::land::Capability::unsupported("unsupported: GitHub is frozen, no writes of any kind");
pub const SERVE: cfrg::land::Capability =
    cfrg::land::Capability::unsupported("unsupported: GitHub is frozen, no writes of any kind");
pub const CONTENTS: cfrg::land::Capability = cfrg::land::Capability::unsupported(
    "unsupported: GitHub is frozen, no consumer reads GitHub repositories through cfrg",
);
pub const OBSERVE: cfrg::land::Capability = cfrg::land::Capability::unsupported(
    "unsupported: GitHub is frozen, no consumer observes GitHub repositories through cfrg",
);

/// Replication: GitHub is frozen. As a sender it can only be `cfrg sync`
/// (adapter-only: reads, never a write to GitHub); it takes no part in a switch.
pub const REPLICATION: cfrg::replicate::Capabilities = cfrg::replicate::Capabilities {
    forge: cfrg::model::Forge::Github,
    push_mirror: cfrg::land::Capability {
        support: cfrg::land::Support::AdapterOnly,
        note: "no native push mirror on any GitHub tier: as a sender it is cfrg sync; GitHub is frozen, so it is read only",
    },
    mirror_key: false,
    pull_mirror: cfrg::land::Capability::unsupported(
        "unsupported: the importer is one-shot and the source-import API is retired",
    ),
    pull_mirror_converts_existing: false,
    receiver_lock: cfrg::land::Capability::unsupported(
        "unsupported: GitHub is frozen, no writes of any kind (private repositories on Free have no protection API anyway)",
    ),
    switch: cfrg::land::Capability::unsupported("unsupported: GitHub is frozen, no writes of any kind"),
    rename: cfrg::land::Capability::unsupported("unsupported: GitHub is frozen, no writes of any kind"),
};

use cfrg::{
    access::{Call, Grants, RoleMap},
    collect::{self, EvidenceSource, Transport},
    model::{Forge, Level},
    profile::ProfileConvention,
    Result,
};
use cqlt::Document;
use serde_json::Value;

/// GitHub role vocabulary.
pub struct GithubRoles;
/// GitHub grant-write API shapes.
pub struct GithubGrants;
/// GitHub evidence dialect.
pub struct GithubEvidence;
/// GitHub profile conventions.
pub struct GithubProfile;

pub static ROLES: GithubRoles = GithubRoles;
pub static GRANTS: GithubGrants = GithubGrants;
pub static EVIDENCE: GithubEvidence = GithubEvidence;
pub static PROFILE: GithubProfile = GithubProfile;

impl RoleMap for GithubRoles {
    fn normalize_role(&self, role: &str) -> Result<Level> {
        match role.trim().to_ascii_lowercase().as_str() {
            "pull" | "triage" | "member" | "read" => Ok(Level::Read),
            "push" | "maintain" | "write" => Ok(Level::Write),
            "admin" | "owner" => Ok(Level::Admin),
            _ => Err(format!(
                "Unknown github role; record a documented role instead of guessing: {role}"
            )
            .into()),
        }
    }
}

impl Grants for GithubGrants {
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
                format!(
                    "orgs/{org}/teams/{}/memberships/{handle}",
                    team.unwrap_or_default()
                )
            } else {
                format!(
                    "repos/{org}/{}/collaborators/{handle}",
                    repo.unwrap_or_default()
                )
            };
            return Call {
                forge: Forge::Github.as_str().into(),
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
                format!(
                    "orgs/{org}/teams/{}/memberships/{handle}",
                    team.unwrap_or_default()
                ),
                "{\"role\":\"member\"}".into(),
                vec![format!("team slug for {org}/{}", team.unwrap_or_default())],
            )
        } else {
            (
                format!(
                    "repos/{org}/{}/collaborators/{handle}",
                    repo.unwrap_or_default()
                ),
                format!(
                    "{{\"permission\":\"{}\"}}",
                    match level {
                        Level::Read => "pull",
                        Level::Write => "push",
                        Level::Admin => "admin",
                    }
                ),
                Vec::new(),
            )
        };
        Call {
            forge: Forge::Github.as_str().into(),
            operation: "set-level".into(),
            method: "PUT".into(),
            path,
            body,
            lookups,
            endpoint_verified: false,
        }
    }
}

impl EvidenceSource for GithubEvidence {
    fn source_label(&self) -> &'static str {
        "Github"
    }

    fn default_api_base(&self) -> Option<&'static str> {
        Some("https://api.github.com")
    }

    fn prefers_gh_login(&self) -> bool {
        true
    }

    fn page_param(&self) -> &'static str {
        "per_page"
    }

    fn scope_login_field(&self) -> &'static str {
        "login"
    }

    fn repos_path(&self, login: &str) -> String {
        format!("orgs/{login}/repos?type=all")
    }

    fn profile_repo(&self) -> &'static str {
        ".github"
    }

    fn repo_homepage_field(&self) -> &'static str {
        "homepage"
    }

    fn org_name_field(&self) -> &'static str {
        "name"
    }

    fn org_website_field(&self) -> &'static str {
        "blog"
    }

    fn branch_commit_field(&self) -> &'static str {
        "sha"
    }

    fn repo_topics(
        &self,
        _transport: &dyn Transport,
        _path: &str,
        row: &Value,
    ) -> Result<Vec<String>> {
        let rows = match row.get("topics").and_then(Value::as_array) {
            Some(rows) => rows,
            None => return Err("Missing topics array".to_owned().into()),
        };
        let mut topics = Vec::new();
        for value in rows {
            match value.as_str() {
                Some(topic) => topics.push(topic.to_owned()),
                None => return Err("Invalid topic".to_owned().into()),
            }
        }
        Ok(topics)
    }

    fn repo_readme(
        &self,
        transport: &dyn Transport,
        path: &str,
        revision: &str,
        _root: &[Value],
    ) -> Result<Document> {
        match transport.get(&format!("{path}/readme?ref={revision}"))? {
            Some(value) => Ok(Document::Present {
                path: collect::text(&value, "path")?.into(),
                bytes: value
                    .get("size")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "Missing README size".to_owned())?,
            }),
            None => Ok(Document::Missing),
        }
    }

    fn org_profile(
        &self,
        transport: &dyn Transport,
        path: &str,
        revision: &str,
        root: Vec<Value>,
    ) -> Result<Document> {
        if root
            .iter()
            .any(|e| e["name"] == "profile" && e["type"] == "dir")
        {
            let entries = collect::directory(
                transport,
                &format!("{path}/contents/profile?ref={revision}"),
            )?;
            // GitHub requires this exact filename for organization profiles.
            let entries = entries
                .into_iter()
                .filter(|e| e["name"] == "README.md")
                .collect::<Vec<_>>();
            collect::listed_document(&entries, "profile/", &["README"])
        } else {
            Ok(Document::Missing)
        }
    }
}

impl ProfileConvention for GithubProfile {
    fn profile_repo(&self) -> Option<&'static str> {
        Some(".github")
    }

    fn readme_path(&self) -> Option<&'static str> {
        Some("profile/README.md")
    }

    fn default_dir(&self) -> Option<&'static str> {
        Some(".github")
    }

    fn frozen_readonly(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_follow_the_github_table() {
        assert_eq!(GithubRoles.normalize_role("pull").unwrap(), Level::Read);
        assert_eq!(GithubRoles.normalize_role("triage").unwrap(), Level::Read);
        assert_eq!(GithubRoles.normalize_role("member").unwrap(), Level::Read);
        assert_eq!(GithubRoles.normalize_role("push").unwrap(), Level::Write);
        assert_eq!(
            GithubRoles.normalize_role("maintain").unwrap(),
            Level::Write
        );
        assert_eq!(GithubRoles.normalize_role("owner").unwrap(), Level::Admin);
        assert_eq!(GithubRoles.normalize_role("ADMIN").unwrap(), Level::Admin);
        assert_eq!(GithubRoles.normalize_role(" WRITE ").unwrap(), Level::Write);
        assert!(GithubRoles.normalize_role("superuser").is_err());
    }

    #[test]
    fn team_grants_use_memberships_repo_grants_use_permissions() {
        let membership = GithubGrants.write_call(
            Some("dev"),
            None,
            Some(Level::Write),
            "acme",
            "alice-gh",
            true,
        );
        assert_eq!(membership.path, "orgs/acme/teams/dev/memberships/alice-gh");
        assert!(membership.body.contains("member"));
        let permission = GithubGrants.write_call(
            None,
            Some("widget"),
            Some(Level::Read),
            "acme",
            "alice-gh",
            false,
        );
        assert_eq!(permission.path, "repos/acme/widget/collaborators/alice-gh");
        assert!(permission.body.contains("pull"));
    }

    #[test]
    fn evidence_dialect_matches_github_api_shapes() {
        assert_eq!(
            GithubEvidence.default_api_base(),
            Some("https://api.github.com")
        );
        assert!(GithubEvidence.prefers_gh_login());
        assert_eq!(GithubEvidence.page_param(), "per_page");
        assert_eq!(GithubEvidence.scope_login_field(), "login");
        assert_eq!(
            GithubEvidence.repos_path("example"),
            "orgs/example/repos?type=all"
        );
        assert_eq!(GithubEvidence.profile_repo(), ".github");
        assert_eq!(GithubEvidence.branch_commit_field(), "sha");
    }

    #[test]
    fn profile_conventions_point_at_dot_github_frozen_read_only() {
        assert_eq!(GithubProfile.profile_repo(), Some(".github"));
        assert_eq!(GithubProfile.readme_path(), Some("profile/README.md"));
        assert_eq!(GithubProfile.default_dir(), Some(".github"));
        assert!(GithubProfile.frozen_readonly());
    }
}
