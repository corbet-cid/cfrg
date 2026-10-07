//! GitLab adapter for cfrg: roles, grants, status, bridge API, profile.
//!
//! Every GitLab-specific rule lives here; core planners stay
//! forge-independent and receive these implementations from the CLI.
//!
//! Rate-limit signals: GitLab answers throttled API use with HTTP 429 and a
//! `Retry-After` delay, and plan-limited calls with HTTP 402. The core paced
//! executor stops a run on the first 401/403/429/402 and records the halt;
//! the status transport cools down on 429 before retrying later.

#![forbid(unsafe_code)]

pub mod native;

/// Declared landing and release capability of this adapter. Never guessed:
/// `unsupported` means the procedure is not implemented here.
pub const LAND: cfrg::land::Capability = cfrg::land::Capability::unsupported("not implemented. GitLab Free has auto_merge (PUT merge_requests/:iid/merge), merge_method ff and a rebase API natively (MATRIX P1); merge trains need Premium, so a serial queue would be cfrg-side");
pub const RELEASE: cfrg::land::Capability = cfrg::land::Capability::unsupported("not implemented. GitLab has release links and a generic package registry natively (MATRIX P3); a release adapter would publish to the registry and link it");
pub const SERVE: cfrg::land::Capability = cfrg::land::Capability::unsupported("not implemented. GitLab Free has project webhooks natively (push, merge request events, token or signing token); no group hooks, so a reconcile sweep stays needed (MATRIX P4)");

use cfrg::{
    access::{Call, Grants, RoleMap},
    bridge::{
        Author, BridgeAuth, BridgeMethod, BridgeRequest, BridgeTransport, GitSource, GitlabBridge,
        GitlabNote, GitlabProject, MergeRequest, MergeRequestRef, MutationPolicy,
    },
    model::{Forge, Level},
    profile::ProfileConvention,
    status::{AuthScheme, State, StatusTarget},
    Result,
};
use serde_json::{json, Value};

/// GitLab role vocabulary.
pub struct GitlabRoles;
/// GitLab grant-write API shapes.
pub struct GitlabGrants;
/// GitLab native status API.
pub struct GitlabStatus;
/// GitLab profile conventions.
pub struct GitlabProfile;

pub static ROLES: GitlabRoles = GitlabRoles;
pub static GRANTS: GitlabGrants = GitlabGrants;
pub static STATUS: GitlabStatus = GitlabStatus;
pub static PROFILE: GitlabProfile = GitlabProfile;

impl RoleMap for GitlabRoles {
    fn normalize_role(&self, role: &str) -> Result<Level> {
        match role.trim().to_ascii_lowercase().as_str() {
            "guest" | "reporter" | "minimal" | "10" | "20" | "read" => Ok(Level::Read),
            "developer" | "30" | "write" => Ok(Level::Write),
            "maintainer" | "owner" | "40" | "50" | "admin" => Ok(Level::Admin),
            _ => Err(format!(
                "Unknown gitlab role; record a documented role instead of guessing: {role}"
            )
            .into()),
        }
    }
}

impl Grants for GitlabGrants {
    fn write_call(
        &self,
        team: Option<&str>,
        repo: Option<&str>,
        level: Option<Level>,
        org: &str,
        handle: &str,
        is_new: bool,
    ) -> Call {
        // Endpoint templates are UNVERIFIED (see docs/access-sync.md).
        if level.is_none() {
            let path = if team.is_some() {
                format!("groups/{org}/members/{handle}")
            } else {
                format!(
                    "projects/{org}/{}/members/{handle}",
                    repo.unwrap_or_default()
                )
            };
            return Call {
                forge: Forge::Gitlab.as_str().into(),
                operation: "revoke".into(),
                method: "DELETE".into(),
                path,
                body: String::new(),
                lookups: Vec::new(),
                endpoint_verified: false,
            };
        }
        let level = level.unwrap_or(Level::Read);
        let access = match level {
            Level::Read => 20,
            Level::Write => 30,
            Level::Admin => 40,
        };
        let (path, body, lookups) = if team.is_some() {
            (
                format!("groups/{org}/members/{handle}"),
                format!("{{\"username\":\"{handle}\",\"access_level\":{access}}}"),
                vec![
                    format!("user id for {handle}"),
                    format!("group id for {org}"),
                ],
            )
        } else {
            (
                format!(
                    "projects/{org}/{}/members/{handle}",
                    repo.unwrap_or_default()
                ),
                format!("{{\"username\":\"{handle}\",\"access_level\":{access}}}"),
                vec![
                    format!("user id for {handle}"),
                    format!("project id for {org}/{}", repo.unwrap_or_default()),
                ],
            )
        };
        // GitLab creates memberships through the collection and updates them
        // through the member; every other forge upserts through one endpoint.
        let (method, path) = if is_new {
            (
                "POST",
                if team.is_some() {
                    format!("groups/{org}/members")
                } else {
                    format!("projects/{org}/{}/members", repo.unwrap_or_default())
                },
            )
        } else {
            ("PUT", path)
        };
        Call {
            forge: Forge::Gitlab.as_str().into(),
            operation: "set-level".into(),
            method: method.into(),
            path,
            body,
            lookups,
            endpoint_verified: false,
        }
    }
}

fn encode(value: &str) -> String {
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

impl StatusTarget for GitlabStatus {
    fn validate(&self, _origin: &str, repository: &str) -> Result<()> {
        // Nested groups are allowed; every segment stays plain.
        let parts: Vec<_> = repository.split('/').collect();
        if parts.len() < 2
            || parts.iter().any(|p| {
                p.is_empty()
                    || *p == "."
                    || *p == ".."
                    || !p
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            })
        {
            return Err("Invalid GitLab repository; want group[/subgroup]/project"
                .to_owned()
                .into());
        }
        Ok(())
    }

    fn auth(&self) -> AuthScheme {
        AuthScheme::PrivateToken
    }

    fn commit_path(&self, repository: &str, sha: &str) -> String {
        format!(
            "/api/v4/projects/{}/repository/commits/{sha}",
            encode(repository)
        )
    }

    fn status_path(&self, repository: &str, sha: &str) -> String {
        format!("/api/v4/projects/{}/statuses/{sha}", encode(repository))
    }

    fn status_body(&self, name: &str, url: &str, state: State) -> Value {
        let state = match state {
            State::Pending => "pending",
            State::Success => "success",
            State::Failure => "failure",
        };
        json!({"state":if state == "failure" {"failed"} else {state},"name":name,"target_url":url,"description":format!("{name}: {state}")})
    }

    fn commit_field(&self) -> &'static str {
        "id"
    }
}

impl ProfileConvention for GitlabProfile {
    fn profile_repo(&self) -> Option<&'static str> {
        Some("gitlab-profile")
    }

    fn readme_path(&self) -> Option<&'static str> {
        Some("README.md")
    }

    fn default_dir(&self) -> Option<&'static str> {
        Some(".gitlab")
    }

    fn frozen_readonly(&self) -> bool {
        false
    }

    fn valid_repo_name(&self, name: &str) -> bool {
        cfrg::profile::plain_name(name) && !name.split('/').any(|part| part.starts_with('.'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_follow_the_gitlab_table() {
        assert_eq!(GitlabRoles.normalize_role("guest").unwrap(), Level::Read);
        assert_eq!(GitlabRoles.normalize_role("reporter").unwrap(), Level::Read);
        assert_eq!(GitlabRoles.normalize_role("minimal").unwrap(), Level::Read);
        assert_eq!(GitlabRoles.normalize_role("10").unwrap(), Level::Read);
        assert_eq!(GitlabRoles.normalize_role("20").unwrap(), Level::Read);
        assert_eq!(
            GitlabRoles.normalize_role("Developer").unwrap(),
            Level::Write
        );
        assert_eq!(GitlabRoles.normalize_role("30").unwrap(), Level::Write);
        assert_eq!(
            GitlabRoles.normalize_role("maintainer").unwrap(),
            Level::Admin
        );
        assert_eq!(GitlabRoles.normalize_role("owner").unwrap(), Level::Admin);
        assert_eq!(GitlabRoles.normalize_role("40").unwrap(), Level::Admin);
        assert_eq!(GitlabRoles.normalize_role("50").unwrap(), Level::Admin);
        assert!(GitlabRoles.normalize_role("superuser").is_err());
    }

    #[test]
    fn grant_posts_collection_change_puts_member() {
        let team = Some("dev");
        let granted =
            GitlabGrants.write_call(team, None, Some(Level::Write), "acme", "alice-gl", true);
        assert_eq!(granted.method, "POST");
        assert_eq!(granted.path, "groups/acme/members");
        let changed =
            GitlabGrants.write_call(team, None, Some(Level::Write), "acme", "alice-gl", false);
        assert_eq!(changed.method, "PUT");
        assert!(changed.path.contains("/members/"));
    }

    #[test]
    fn status_paths_encode_nested_groups() {
        assert_eq!(
            GitlabStatus.commit_path("group/sub/repo", "a"),
            "/api/v4/projects/group%2Fsub%2Frepo/repository/commits/a"
        );
        assert_eq!(GitlabStatus.commit_field(), "id");
        assert!(GitlabStatus
            .validate("https://gitlab.example", "group/sub/repo")
            .is_ok());
        assert!(GitlabStatus
            .validate("https://gitlab.example", "repo")
            .is_err());
    }

    #[test]
    fn profile_conventions_use_gitlab_profile_and_reject_leading_dots() {
        assert_eq!(GitlabProfile.profile_repo(), Some("gitlab-profile"));
        assert_eq!(GitlabProfile.readme_path(), Some("README.md"));
        assert_eq!(GitlabProfile.default_dir(), Some(".gitlab"));
        assert!(GitlabProfile.valid_repo_name("group/widget"));
        assert!(!GitlabProfile.valid_repo_name("group/.hidden"));
        assert!(!GitlabProfile.valid_repo_name(""));
    }
}

/// GitLab side of the contribution bridge: merge-request reads plus the two
/// feedback writes (note post, MR close). Every endpoint, payload shape and
/// response field below is GitLab-specific; the core orchestrator only names
/// these typed operations.
///
/// Allowed mutations (the complete set — there is no generic mutation path):
/// `post_note` (POST `…/notes`) and `close_merge_request` (PUT the MR with
/// `{"state_event":"close"}`). GitLab answers throttled use with HTTP 429
/// and plan-limited calls with HTTP 402; both stop a paced run.
pub struct Gitlab;
pub static GITLAB: Gitlab = Gitlab;

fn request(origin: &str, method: BridgeMethod, path: String, body: Option<Value>) -> BridgeRequest {
    BridgeRequest {
        origin: origin.into(),
        token_env: "CFRG_BRIDGE_GITLAB_TOKEN",
        auth: BridgeAuth::PrivateToken,
        method,
        path,
        body,
    }
}

fn positive(value: &Value, name: &str) -> Result<u64> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("GitLab response misses positive {name}").into())
}

fn text(value: &Value, name: &str) -> Result<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("GitLab response misses string {name}").into())
}

fn api(path: &str) -> String {
    format!("/api/v4/{path}")
}

impl GitlabBridge for Gitlab {
    fn mr_project(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        project: u64,
    ) -> Result<GitlabProject> {
        let value = transport.execute(&request(
            origin,
            BridgeMethod::Get,
            api(&format!("projects/{project}")),
            None,
        ))?;
        Ok(GitlabProject {
            id: positive(&value, "id")?,
            path_with_namespace: text(&value, "path_with_namespace")?,
            visibility: text(&value, "visibility")?,
        })
    }

    fn merge_request(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        project: u64,
        iid: u64,
    ) -> Result<MergeRequest> {
        let value = transport.execute(&request(
            origin,
            BridgeMethod::Get,
            api(&format!("projects/{project}/merge_requests/{iid}")),
            None,
        ))?;
        Ok(MergeRequest {
            iid: positive(&value, "iid")?,
            project_id: positive(&value, "project_id")?,
            target_project_id: positive(&value, "target_project_id")?,
            state: text(&value, "state")?,
            title: text(&value, "title")?,
            sha: text(&value, "sha")?,
            target_branch: text(&value, "target_branch")?,
            author: Author {
                id: positive(
                    value
                        .get("author")
                        .ok_or_else(|| "GitLab MR misses author".to_owned())?,
                    "id",
                )?,
                username: text(
                    value
                        .get("author")
                        .ok_or_else(|| "GitLab MR misses author".to_owned())?,
                    "username",
                )?,
            },
        })
    }

    fn open_merge_requests(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        project: u64,
    ) -> Result<Vec<MergeRequestRef>> {
        let values = cfrg::bridge::fetch_pages(
            transport,
            origin,
            "CFRG_BRIDGE_GITLAB_TOKEN",
            BridgeAuth::PrivateToken,
            &api(&format!(
                "projects/{project}/merge_requests?state=opened&scope=all&order_by=created_at&sort=asc"
            )),
            "per_page",
        )?;
        values
            .iter()
            .map(|value| {
                Ok(MergeRequestRef {
                    iid: positive(value, "iid")?,
                })
            })
            .collect()
    }

    fn merge_request_notes(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        project: u64,
        iid: u64,
    ) -> Result<Vec<GitlabNote>> {
        let values = cfrg::bridge::fetch_pages(
            transport,
            origin,
            "CFRG_BRIDGE_GITLAB_TOKEN",
            BridgeAuth::PrivateToken,
            &api(&format!("projects/{project}/merge_requests/{iid}/notes")),
            "per_page",
        )?;
        values
            .iter()
            .map(|value| {
                Ok(GitlabNote {
                    id: positive(value, "id").ok(),
                    body: value.get("body").and_then(Value::as_str).map(str::to_owned),
                    author_id: value
                        .get("author")
                        .and_then(|author| author.get("id"))
                        .and_then(Value::as_u64),
                })
            })
            .collect()
    }

    fn post_note(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        project: u64,
        iid: u64,
        body: &str,
    ) -> Result<GitlabNote> {
        let value = transport.execute(&request(
            origin,
            BridgeMethod::Post,
            api(&format!("projects/{project}/merge_requests/{iid}/notes")),
            Some(json!({"body": body})),
        ))?;
        Ok(GitlabNote {
            id: Some(positive(&value, "id")?),
            body: value.get("body").and_then(Value::as_str).map(str::to_owned),
            author_id: value
                .get("author")
                .and_then(|author| author.get("id"))
                .and_then(Value::as_u64),
        })
    }

    fn close_merge_request(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        project: u64,
        iid: u64,
    ) -> Result<()> {
        transport.execute(&request(
            origin,
            BridgeMethod::Put,
            api(&format!("projects/{project}/merge_requests/{iid}")),
            Some(json!({"state_event": "close"})),
        ))?;
        Ok(())
    }

    fn current_user_id(&self, transport: &mut dyn BridgeTransport, origin: &str) -> Result<u64> {
        let value = transport.execute(&request(origin, BridgeMethod::Get, api("user"), None))?;
        positive(&value, "id")
    }

    fn policy(&self, origin: &str, project: u64) -> Box<dyn MutationPolicy> {
        Box::new(GitlabPolicy {
            origin: origin.into(),
            project,
        })
    }

    fn git_source(&self, origin: &str, repository: &str, iid: u64) -> GitSource {
        GitSource {
            url: format!("{origin}/{repository}.git"),
            token_env: Some("CFRG_BRIDGE_GITLAB_TOKEN"),
            username: "oauth2".into(),
            mr_ref: format!("refs/merge-requests/{iid}/head"),
        }
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::*;
    use cfrg::bridge::BridgeTransport;
    use std::collections::BTreeMap;

    struct Recorder {
        requests: Vec<BridgeRequest>,
        responses: BTreeMap<String, Value>,
    }
    impl BridgeTransport for Recorder {
        fn execute(&mut self, request: &BridgeRequest) -> Result<Value> {
            self.requests.push(BridgeRequest {
                origin: request.origin.clone(),
                token_env: request.token_env,
                auth: request.auth,
                method: request.method,
                path: request.path.clone(),
                body: request.body.clone(),
            });
            Ok(self
                .responses
                .get(&request.path)
                .cloned()
                .unwrap_or(json!({})))
        }
    }

    fn transport() -> Recorder {
        let mut responses = BTreeMap::new();
        responses.insert(
            "/api/v4/projects/42".into(),
            json!({"id":42,"path_with_namespace":"team/project","visibility":"public"}),
        );
        responses.insert(
            "/api/v4/projects/42/merge_requests/7".into(),
            json!({"iid":7,"project_id":42,"target_project_id":42,"state":"opened","title":"A contribution","sha":"a".repeat(40),"target_branch":"main","author":{"id":8,"username":"contributor"}}),
        );
        responses.insert(
            "/api/v4/projects/42/merge_requests?state=opened&scope=all&order_by=created_at&sort=asc&per_page=50&page=1".into(),
            json!([{"iid":7}]),
        );
        responses.insert(
            "/api/v4/projects/42/merge_requests?state=opened&scope=all&order_by=created_at&sort=asc&per_page=50&page=2".into(),
            json!([]),
        );
        responses.insert("/api/v4/user".into(), json!({"id":99}));
        responses.insert(
            "/api/v4/projects/42/merge_requests/7/notes".into(),
            json!({"id":100,"body":"hello","author":{"id":99}}),
        );
        Recorder {
            requests: Vec::new(),
            responses,
        }
    }

    #[test]
    fn merge_request_reads_use_project_paths_and_parse_strictly() {
        let mut transport = transport();
        let mr = GITLAB
            .merge_request(&mut transport, "https://gitlab.example", 42, 7)
            .unwrap();
        assert_eq!(mr.iid, 7);
        assert_eq!(mr.author.username, "contributor");
        assert_eq!(transport.requests.len(), 1);
        let request = &transport.requests[0];
        assert_eq!(request.method, BridgeMethod::Get);
        assert_eq!(request.path, "/api/v4/projects/42/merge_requests/7");
        assert_eq!(request.token_env, "CFRG_BRIDGE_GITLAB_TOKEN");
        assert_eq!(request.auth, BridgeAuth::PrivateToken);
        let refs = GITLAB
            .open_merge_requests(&mut transport, "https://gitlab.example", 42)
            .unwrap();
        assert_eq!(refs, vec![MergeRequestRef { iid: 7 }]);
        assert!(transport.requests[2].path.contains("per_page=50"));
    }

    #[test]
    fn feedback_writes_hit_only_notes_post_and_mr_close() {
        let mut transport = transport();
        let note = GITLAB
            .post_note(&mut transport, "https://gitlab.example", 42, 7, "hello")
            .unwrap();
        let close_paths: Vec<_> = transport
            .requests
            .iter()
            .map(|request| (request.method, request.path.clone()))
            .collect();
        assert_eq!(
            close_paths,
            vec![(
                BridgeMethod::Post,
                "/api/v4/projects/42/merge_requests/7/notes".to_string()
            )]
        );
        assert_eq!(note.body.as_deref(), Some("hello"));
        assert_eq!(note.author_id, Some(99));
        assert_eq!(note.id, Some(100));
        GITLAB
            .close_merge_request(&mut transport, "https://gitlab.example", 42, 7)
            .unwrap();
        assert_eq!(transport.requests[1].method, BridgeMethod::Put);
        assert_eq!(
            transport.requests[1].body,
            Some(json!({"state_event": "close"}))
        );
    }

    #[test]
    fn malformed_responses_fail_closed() {
        let mut transport = transport();
        transport.responses.insert(
            "/api/v4/projects/42/merge_requests/7".into(),
            json!({"iid":0,"project_id":42}),
        );
        assert!(GITLAB
            .merge_request(&mut transport, "https://gitlab.example", 42, 7)
            .is_err());
    }

    #[test]
    fn git_source_points_at_the_mr_head_ref() {
        let source = GITLAB.git_source("https://gitlab.example", "team/project", 7);
        assert_eq!(source.url, "https://gitlab.example/team/project.git");
        assert_eq!(source.mr_ref, "refs/merge-requests/7/head");
        assert_eq!(source.token_env, Some("CFRG_BRIDGE_GITLAB_TOKEN"));
        assert_eq!(source.username, "oauth2");
    }
}

/// Authorization scope for exactly one GitLab deployment: the validated
/// origin and project id. Only requests to that origin, with this forge's
/// credential, reach the network — reads under `/api/v4/`, and exactly the
/// two feedback mutations with numeric merge-request identities. Anything
/// else fails closed, including cross-origin requests, credential mismatch
/// and path tricks.
struct GitlabPolicy {
    origin: String,
    project: u64,
}

fn numeric_idiom(segment: &str) -> Option<u64> {
    if segment.is_empty() || !segment.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    segment.parse::<u64>().ok().filter(|n| *n > 0)
}

impl MutationPolicy for GitlabPolicy {
    fn authorize(&self, request: &BridgeRequest) -> Result<()> {
        if request.origin != self.origin {
            return Err("GitLab scope rejects foreign origin".into());
        }
        if request.token_env != "CFRG_BRIDGE_GITLAB_TOKEN" {
            return Err("GitLab scope rejects foreign credential".into());
        }
        cfrg::bridge::reject_path_tricks(&request.path)?;
        let prefix = format!("/api/v4/projects/{}/merge_requests/", self.project);
        match request.method {
            BridgeMethod::Get => {
                if request.path.starts_with("/api/v4/") {
                    Ok(())
                } else {
                    Err("GitLab scope refuses reads outside /api/v4/".into())
                }
            }
            BridgeMethod::Post => {
                let rest = request.path.strip_prefix(&prefix).ok_or_else(|| {
                    "GitLab scope allows note posts on this project only".to_owned()
                })?;
                match rest.split_once('/') {
                    Some((iid, "notes")) if numeric_idiom(iid).is_some() => Ok(()),
                    _ => Err("GitLab scope refuses this POST".into()),
                }
            }
            BridgeMethod::Put => {
                let rest = request.path.strip_prefix(&prefix).ok_or_else(|| {
                    "GitLab scope allows MR closes on this project only".to_owned()
                })?;
                match numeric_idiom(rest) {
                    Some(_) => Ok(()),
                    None => Err("GitLab scope refuses this PUT".into()),
                }
            }
            BridgeMethod::Patch => Err("GitLab scope never patches".into()),
        }
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use cfrg::bridge::{BridgeAuth, BridgeMethod, BridgeRequest, MutationPolicy};

    fn policy() -> Box<dyn MutationPolicy> {
        GITLAB.policy("https://gitlab.example", 42)
    }

    fn request(method: BridgeMethod, path: &str) -> BridgeRequest {
        request_with(
            "https://gitlab.example",
            "CFRG_BRIDGE_GITLAB_TOKEN",
            method,
            path,
        )
    }

    fn request_with(
        origin: &str,
        token: &'static str,
        method: BridgeMethod,
        path: &str,
    ) -> BridgeRequest {
        BridgeRequest {
            origin: origin.into(),
            token_env: token,
            auth: BridgeAuth::PrivateToken,
            method,
            path: path.into(),
            body: None,
        }
    }

    #[test]
    fn exact_feedback_mutations_pass() {
        let policy = policy();
        assert!(policy
            .authorize(&request(
                BridgeMethod::Post,
                "/api/v4/projects/42/merge_requests/7/notes"
            ))
            .is_ok());
        assert!(policy
            .authorize(&request(
                BridgeMethod::Put,
                "/api/v4/projects/42/merge_requests/7"
            ))
            .is_ok());
        assert!(policy
            .authorize(&request(BridgeMethod::Get, "/api/v4/projects/42"))
            .is_ok());
    }

    #[test]
    fn cross_origin_credential_mismatch_and_path_tricks_fail() {
        let policy = policy();
        // Foreign origin with the right credential.
        assert!(policy
            .authorize(&request_with(
                "https://evil.example",
                "CFRG_BRIDGE_GITLAB_TOKEN",
                BridgeMethod::Get,
                "/api/v4/projects/42"
            ))
            .is_err());
        // Right origin with the other forge's credential.
        assert!(policy
            .authorize(&request_with(
                "https://gitlab.example",
                "CFRG_BRIDGE_FORGEJO_TOKEN",
                BridgeMethod::Get,
                "/api/v4/projects/42"
            ))
            .is_err());
        // Reads outside the versioned API root.
        assert!(policy
            .authorize(&request(BridgeMethod::Get, "/api/v3/projects/42"))
            .is_err());
        // Traversal, doubling, whitespace.
        for path in [
            "/api/v4/projects/42/merge_requests/../7",
            "/api/v4//projects/42",
            "/api/v4/projects/42/merge_requests/7/notes ",
        ] {
            assert!(
                policy.authorize(&request(BridgeMethod::Get, path)).is_err(),
                "{path}"
            );
        }
        // Non-numeric or foreign merge-request identities.
        for (method, path) in [
            (
                BridgeMethod::Post,
                "/api/v4/projects/42/merge_requests/abc/notes",
            ),
            (
                BridgeMethod::Post,
                "/api/v4/projects/42/merge_requests/7/note",
            ),
            (
                BridgeMethod::Post,
                "/api/v4/projects/43/merge_requests/7/notes",
            ),
            (BridgeMethod::Put, "/api/v4/projects/42/merge_requests/0"),
            (
                BridgeMethod::Put,
                "/api/v4/projects/42/merge_requests/7/notes",
            ),
            (BridgeMethod::Patch, "/api/v4/projects/42/merge_requests/7"),
        ] {
            assert!(policy.authorize(&request(method, path)).is_err(), "{path}");
        }
    }
}

#[cfg(test)]
mod note_parse_tests {
    use super::*;
    use cfrg::bridge::BridgeTransport;
    use serde_json::json;

    struct Notes {
        notes: Value,
    }
    impl BridgeTransport for Notes {
        fn execute(&mut self, request: &BridgeRequest) -> Result<Value> {
            if request.path.ends_with("page=1") {
                Ok(self.notes.clone())
            } else {
                Ok(json!([]))
            }
        }
    }

    #[test]
    fn bodyless_and_authorless_notes_parse_to_nones_for_filtering() {
        let mut transport = Notes {
            notes: json!([
                {"id": 1},
                {"id": 2, "body": "text", "author": {"id": 3}},
            ]),
        };
        let notes = GITLAB
            .merge_request_notes(&mut transport, "https://gitlab.example", 42, 7)
            .unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].id, Some(1));
        assert_eq!(notes[0].body, None);
        assert_eq!(notes[0].author_id, None);
        assert_eq!(notes[1].author_id, Some(3));
    }
}

#[cfg(test)]
mod status_body_tests {
    use super::*;

    #[test]
    fn bodies_map_failure_and_keep_name_target_description() {
        // The GitLab state field uses "failed"; the description keeps the
        // pre-split internal word ("failure") byte-identical. Flagged for
        // reviewer decision: unifying them changes visible wire text.
        for (state, word, text) in [
            (State::Pending, "pending", "pending"),
            (State::Success, "success", "success"),
            (State::Failure, "failed", "failure"),
        ] {
            let body = GitlabStatus.status_body("ccid/verify", "https://ci.example/1", state);
            assert_eq!(body["state"], word);
            assert_eq!(body["name"], "ccid/verify");
            assert_eq!(body["target_url"], "https://ci.example/1");
            assert_eq!(body["description"], format!("ccid/verify: {text}"));
        }
    }
}
