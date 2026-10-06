//! Forgejo adapter for cfrg: roles, grants, status, evidence, profile.
//!
//! Every Forgejo-specific rule lives here; core planners stay
//! forge-independent and receive these implementations from the CLI.
//!
//! Rate-limit signals: Forgejo answers throttled API use with HTTP 429 and an
//! optional `Retry-After` delay. The core paced executor stops a run on the
//! first 401/403/429/402 and records the halt; the status transport cools
//! down on 429 before retrying later.

#![forbid(unsafe_code)]

use cfrg::{
    access::{Call, Grants, RoleMap},
    bridge::{
        BridgeAuth, BridgeMethod, BridgeRequest, BridgeTransport, CollaboratorPermission,
        ForgejoBridge, ForgejoRepo, GitDest, MutationPolicy, PullRequest, PullReview,
    },
    collect::{self, EvidenceSource, Transport},
    model::{Forge, Level},
    profile::ProfileConvention,
    status::{AuthScheme, State, StatusTarget},
    Result,
};
use cqlt::Document;
use serde_json::{json, Value};

/// Forgejo role vocabulary.
pub struct ForgejoRoles;
/// Forgejo grant-write API shapes.
pub struct ForgejoGrants;
/// Forgejo native status API.
pub struct ForgejoStatus;
/// Forgejo evidence dialect.
pub struct ForgejoEvidence;
/// Forgejo profile conventions.
pub struct ForgejoProfile;

pub static ROLES: ForgejoRoles = ForgejoRoles;
pub static GRANTS: ForgejoGrants = ForgejoGrants;
pub static STATUS: ForgejoStatus = ForgejoStatus;
pub static EVIDENCE: ForgejoEvidence = ForgejoEvidence;
pub static PROFILE: ForgejoProfile = ForgejoProfile;

impl RoleMap for ForgejoRoles {
    fn normalize_role(&self, role: &str) -> Result<Level> {
        match role.trim().to_ascii_lowercase().as_str() {
            "read" => Ok(Level::Read),
            "write" => Ok(Level::Write),
            "admin" | "owner" => Ok(Level::Admin),
            _ => Err(format!(
                "Unknown forgejo role; record a documented role instead of guessing: {role}"
            )
            .into()),
        }
    }
}

impl Grants for ForgejoGrants {
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
                format!("teams/{}/members/{handle}", team.unwrap_or_default())
            } else {
                format!(
                    "repos/{org}/{}/collaborators/{handle}",
                    repo.unwrap_or_default()
                )
            };
            return Call {
                forge: Forge::Forgejo.as_str().into(),
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
                format!("teams/{}/members/{handle}", team.unwrap_or_default()),
                String::new(),
                vec![format!("team id for {org}/{}", team.unwrap_or_default())],
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
                        Level::Read => "read",
                        Level::Write => "write",
                        Level::Admin => "admin",
                    }
                ),
                Vec::new(),
            )
        };
        Call {
            forge: Forge::Forgejo.as_str().into(),
            operation: "set-level".into(),
            method: "PUT".into(),
            path,
            body,
            lookups,
            endpoint_verified: false,
        }
    }
}

impl StatusTarget for ForgejoStatus {
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
            return Err("Invalid Forgejo repository; want owner/name"
                .to_owned()
                .into());
        }
        if origin.is_empty() {
            return Err("Invalid Forgejo origin".to_owned().into());
        }
        Ok(())
    }

    fn auth(&self) -> AuthScheme {
        AuthScheme::Token
    }

    fn commit_path(&self, repository: &str, sha: &str) -> String {
        format!("/api/v1/repos/{repository}/git/commits/{sha}")
    }

    fn status_path(&self, repository: &str, sha: &str) -> String {
        format!("/api/v1/repos/{repository}/statuses/{sha}")
    }

    fn status_body(&self, name: &str, url: &str, state: State) -> Value {
        let state = match state {
            State::Pending => "pending",
            State::Success => "success",
            State::Failure => "failure",
        };
        json!({"state":state,"context":name,"target_url":url,"description":format!("{name}: {state}")})
    }

    fn commit_field(&self) -> &'static str {
        "sha"
    }
}

impl EvidenceSource for ForgejoEvidence {
    fn source_label(&self) -> &'static str {
        "Forgejo"
    }

    fn default_api_base(&self) -> Option<&'static str> {
        None
    }

    fn prefers_gh_login(&self) -> bool {
        false
    }

    fn page_param(&self) -> &'static str {
        "limit"
    }

    fn scope_login_field(&self) -> &'static str {
        "name"
    }

    fn repos_path(&self, login: &str) -> String {
        format!("orgs/{login}/repos")
    }

    fn profile_repo(&self) -> &'static str {
        ".profile"
    }

    fn repo_homepage_field(&self) -> &'static str {
        "website"
    }

    fn org_name_field(&self) -> &'static str {
        "full_name"
    }

    fn org_website_field(&self) -> &'static str {
        "website"
    }

    fn branch_commit_field(&self) -> &'static str {
        "id"
    }

    fn repo_topics(
        &self,
        transport: &dyn Transport,
        path: &str,
        _row: &Value,
    ) -> Result<Vec<String>> {
        let response = collect::required(transport, &format!("{path}/topics"))?;
        let rows = match response.get("topics").and_then(Value::as_array) {
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
        _transport: &dyn Transport,
        _path: &str,
        _revision: &str,
        root: &[Value],
    ) -> Result<Document> {
        collect::listed_document(root, "", &["README"])
    }

    fn org_profile(
        &self,
        _transport: &dyn Transport,
        _path: &str,
        _revision: &str,
        root: Vec<Value>,
    ) -> Result<Document> {
        let entries = root
            .into_iter()
            .filter(|e| e["name"] == "README.md")
            .collect::<Vec<_>>();
        collect::listed_document(&entries, "", &["README"])
    }
}

impl ProfileConvention for ForgejoProfile {
    fn profile_repo(&self) -> Option<&'static str> {
        Some(".profile")
    }

    fn readme_path(&self) -> Option<&'static str> {
        Some("README.md")
    }

    fn default_dir(&self) -> Option<&'static str> {
        Some(".forgejo")
    }

    fn frozen_readonly(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfrg::model::Forge;

    #[test]
    fn roles_follow_the_forgejo_table() {
        assert_eq!(ForgejoRoles.normalize_role("read").unwrap(), Level::Read);
        assert_eq!(ForgejoRoles.normalize_role("write").unwrap(), Level::Write);
        assert_eq!(ForgejoRoles.normalize_role("admin").unwrap(), Level::Admin);
        assert_eq!(ForgejoRoles.normalize_role("owner").unwrap(), Level::Admin);
        assert_eq!(
            ForgejoRoles.normalize_role(" WRITE ").unwrap(),
            Level::Write
        );
        assert!(ForgejoRoles.normalize_role("maintainer").is_err());
        assert!(ForgejoRoles.normalize_role("superuser").is_err());
    }

    #[test]
    fn team_grants_address_members_repo_grants_collaborators() {
        let member = ForgejoGrants.write_call(
            Some("dev"),
            None,
            Some(Level::Write),
            "acme",
            "alice-fj",
            true,
        );
        assert_eq!(member.method, "PUT");
        assert_eq!(member.path, "teams/dev/members/alice-fj");
        assert!(member.body.is_empty());
        let collaborator = ForgejoGrants.write_call(
            None,
            Some("widget"),
            Some(Level::Write),
            "acme",
            "alice-fj",
            false,
        );
        assert_eq!(
            collaborator.path,
            "repos/acme/widget/collaborators/alice-fj"
        );
        assert!(collaborator.body.contains("write"));
        let revoke =
            ForgejoGrants.write_call(None, Some("widget"), None, "acme", "alice-fj", false);
        assert_eq!(revoke.method, "DELETE");
        assert_eq!(revoke.operation, "revoke");
    }

    #[test]
    fn status_paths_use_the_v1_api() {
        assert_eq!(
            ForgejoStatus.commit_path("team/repo", "a"),
            "/api/v1/repos/team/repo/git/commits/a"
        );
        assert_eq!(ForgejoStatus.commit_field(), "sha");
        assert!(ForgejoStatus
            .validate("https://forge.example", "team/repo")
            .is_ok());
        assert!(ForgejoStatus
            .validate("https://forge.example", "a/b/c")
            .is_err());
    }

    #[test]
    fn profile_conventions_point_at_dot_profile_and_forgejo_defaults() {
        assert_eq!(ForgejoProfile.profile_repo(), Some(".profile"));
        assert_eq!(ForgejoProfile.readme_path(), Some("README.md"));
        assert_eq!(ForgejoProfile.default_dir(), Some(".forgejo"));
        assert!(!ForgejoProfile.frozen_readonly());
    }

    #[test]
    fn evidence_dialect_matches_forgejo_api_shapes() {
        assert_eq!(ForgejoEvidence.page_param(), "limit");
        assert_eq!(ForgejoEvidence.scope_login_field(), "name");
        assert_eq!(ForgejoEvidence.repos_path("example"), "orgs/example/repos");
        assert_eq!(ForgejoEvidence.profile_repo(), ".profile");
        assert_eq!(ForgejoEvidence.branch_commit_field(), "id");
        assert_eq!(ForgejoEvidence.default_api_base(), None);
        assert!(!ForgejoEvidence.prefers_gh_login());
    }

    #[test]
    fn adapter_covers_forgejo() {
        assert_eq!(Forge::Forgejo.as_str(), "forgejo");
    }
}

/// Forgejo side of the contribution bridge: repository, pull and review
/// reads plus the two import writes (pull create, predecessor close). Every
/// endpoint, payload shape and response field below is Forgejo-specific; the
/// core orchestrator only names these typed operations.
///
/// Allowed mutations (the complete set — there is no generic mutation path):
/// `create_pull` (POST `…/pulls`) and `close_pull` (PATCH one pull with
/// `{"state":"closed",…}`). Forgejo answers throttled API use with HTTP 429.
pub struct Forgejo;
pub static FORGEJO: Forgejo = Forgejo;

fn request(origin: &str, method: BridgeMethod, path: String, body: Option<Value>) -> BridgeRequest {
    BridgeRequest {
        origin: origin.into(),
        token_env: "CFRG_BRIDGE_FORGEJO_TOKEN",
        auth: BridgeAuth::Token,
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
        .ok_or_else(|| format!("Forgejo response misses positive {name}").into())
}

fn text(value: &Value, name: &str) -> Result<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("Forgejo response misses string {name}").into())
}

fn api(path: &str) -> String {
    format!("/api/v1/{path}")
}

fn parse_repo(value: &Value) -> Result<ForgejoRepo> {
    let parent = match value.get("parent") {
        None => None,
        Some(parent) => Some(positive(parent, "id")?),
    };
    Ok(ForgejoRepo {
        id: positive(value, "id")?,
        full_name: text(value, "full_name")?,
        private: value
            .get("private")
            .and_then(Value::as_bool)
            .ok_or_else(|| "Forgejo response misses boolean private".to_owned())?,
        has_actions: value
            .get("has_actions")
            .and_then(Value::as_bool)
            .ok_or_else(|| "Forgejo response misses boolean has_actions".to_owned())?,
        fork: value
            .get("fork")
            .and_then(Value::as_bool)
            .ok_or_else(|| "Forgejo response misses boolean fork".to_owned())?,
        parent_id: parent,
        owner_id: positive(
            value
                .get("owner")
                .ok_or_else(|| "Forgejo response misses owner".to_owned())?,
            "id",
        )?,
    })
}

fn parse_pull(value: &Value) -> Result<PullRequest> {
    let head = value
        .get("head")
        .ok_or_else(|| "Forgejo pull misses head".to_owned())?;
    let base = value
        .get("base")
        .ok_or_else(|| "Forgejo pull misses base".to_owned())?;
    let head_repo = head
        .get("repo")
        .ok_or_else(|| "Forgejo pull misses head repo".to_owned())?;
    let base_repo = base
        .get("repo")
        .ok_or_else(|| "Forgejo pull misses base repo".to_owned())?;
    Ok(PullRequest {
        number: positive(value, "number")?,
        state: text(value, "state")?,
        merged: value.get("merged").and_then(Value::as_bool),
        head_ref: text(head, "ref")?,
        head_repo_id: positive(head_repo, "id")?,
        head_sha: text(head, "sha")?,
        base_ref: text(base, "ref")?,
        base_repo_id: positive(base_repo, "id")?,
        base_sha: text(base, "sha")?,
        user_id: positive(
            value
                .get("user")
                .ok_or_else(|| "Forgejo pull misses user".to_owned())?,
            "id",
        )?,
        body: value.get("body").and_then(Value::as_str).map(str::to_owned),
    })
}

impl ForgejoBridge for Forgejo {
    fn repository(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        path: &str,
    ) -> Result<ForgejoRepo> {
        parse_repo(&transport.execute(&request(
            origin,
            BridgeMethod::Get,
            api(&format!("repos/{path}")),
            None,
        ))?)
    }

    fn current_user_id(&self, transport: &mut dyn BridgeTransport, origin: &str) -> Result<u64> {
        let value = transport.execute(&request(origin, BridgeMethod::Get, api("user"), None))?;
        positive(&value, "id")
    }

    fn webhooks_empty(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        path: &str,
    ) -> Result<bool> {
        let values = cfrg::bridge::fetch_pages(
            transport,
            origin,
            "CFRG_BRIDGE_FORGEJO_TOKEN",
            BridgeAuth::Token,
            &api(&format!("{path}/hooks")),
            "limit",
        )?;
        Ok(values.is_empty())
    }

    fn pulls(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        repository: &str,
    ) -> Result<Vec<PullRequest>> {
        let values = cfrg::bridge::fetch_pages(
            transport,
            origin,
            "CFRG_BRIDGE_FORGEJO_TOKEN",
            BridgeAuth::Token,
            &api(&format!("repos/{repository}/pulls?state=all")),
            "limit",
        )?;
        values.iter().map(parse_pull).collect()
    }

    fn pull(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        repository: &str,
        index: u64,
    ) -> Result<PullRequest> {
        parse_pull(&transport.execute(&request(
            origin,
            BridgeMethod::Get,
            api(&format!("repos/{repository}/pulls/{index}")),
            None,
        ))?)
    }

    fn create_pull(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        repository: &str,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequest> {
        parse_pull(&transport.execute(&request(
            origin,
            BridgeMethod::Post,
            api(&format!("repos/{repository}/pulls")),
            Some(json!({"head": head, "base": base, "title": title, "body": body})),
        ))?)
    }

    fn close_pull(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        repository: &str,
        index: u64,
        body: &str,
    ) -> Result<PullRequest> {
        parse_pull(&transport.execute(&request(
            origin,
            BridgeMethod::Patch,
            api(&format!("repos/{repository}/pulls/{index}")),
            Some(json!({"state": "closed", "body": body})),
        ))?)
    }

    fn pull_review(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        repository: &str,
        index: u64,
        review: u64,
    ) -> Result<PullReview> {
        let value = transport.execute(&request(
            origin,
            BridgeMethod::Get,
            api(&format!(
                "repos/{repository}/pulls/{index}/reviews/{review}"
            )),
            None,
        ))?;
        let user = value
            .get("user")
            .ok_or_else(|| "Forgejo review misses user".to_owned())?;
        Ok(PullReview {
            id: positive(&value, "id")?,
            user_id: positive(user, "id")?,
            login: text(user, "login")?,
            state: text(&value, "state")?,
            dismissed: value
                .get("dismissed")
                .and_then(Value::as_bool)
                .ok_or_else(|| "Forgejo review misses boolean dismissed".to_owned())?,
            stale: value
                .get("stale")
                .and_then(Value::as_bool)
                .ok_or_else(|| "Forgejo review misses boolean stale".to_owned())?,
            commit_id: text(&value, "commit_id")?,
            body: text(&value, "body")?,
        })
    }

    fn collaborator_permission(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        repository: &str,
        user: &str,
    ) -> Result<CollaboratorPermission> {
        let value = transport.execute(&request(
            origin,
            BridgeMethod::Get,
            api(&format!(
                "repos/{repository}/collaborators/{user}/permission"
            )),
            None,
        ))?;
        Ok(CollaboratorPermission {
            user_id: positive(
                value
                    .get("user")
                    .ok_or_else(|| "Forgejo permission misses user".to_owned())?,
                "id",
            )?,
            permission: text(&value, "permission")?,
        })
    }

    fn policy(&self, origin: &str, primary: &str) -> Box<dyn MutationPolicy> {
        Box::new(ForgejoPolicy {
            origin: origin.into(),
            primary: primary.into(),
        })
    }

    fn git_dest(&self, origin: &str, repository: &str) -> GitDest {
        GitDest {
            url: format!("{origin}/{repository}.git"),
            token_env: Some("CFRG_BRIDGE_FORGEJO_TOKEN"),
            username: repository.split('/').next().unwrap_or_default().into(),
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

    fn pull(number: u64) -> Value {
        json!({"number":number,"state":"open","merged":false,"body":"<!-- ccid-pr-bridge:v1:key -->\ntext",
            "head":{"ref":"ccid-import/gitlab/key","repo":{"id":11},"sha":"a".repeat(40)},
            "base":{"ref":"main","repo":{"id":10},"sha":"b".repeat(40)},
            "user":{"id":12}})
    }

    fn transport() -> Recorder {
        let mut responses = BTreeMap::new();
        responses.insert(
            "/api/v1/repos/team/project".into(),
            json!({"id":10,"full_name":"team/project","private":false,"has_actions":false,"fork":false,"owner":{"id":12}}),
        );
        responses.insert("/api/v1/user".into(), json!({"id":12}));
        responses.insert("/api/v1/repos/team/project/pulls/20".into(), pull(20));
        Recorder {
            requests: Vec::new(),
            responses,
        }
    }

    #[test]
    fn pull_reads_parse_identity_fields_strictly() {
        let mut transport = transport();
        let pull = FORGEJO
            .pull(&mut transport, "https://forge.example", "team/project", 20)
            .unwrap();
        assert_eq!(pull.number, 20);
        assert_eq!(pull.head_repo_id, 11);
        assert_eq!(pull.merged, Some(false));
        assert_eq!(
            pull.body.as_deref(),
            Some("<!-- ccid-pr-bridge:v1:key -->\ntext")
        );
        let request = &transport.requests[0];
        assert_eq!(request.method, BridgeMethod::Get);
        assert_eq!(request.token_env, "CFRG_BRIDGE_FORGEJO_TOKEN");
        assert_eq!(request.auth, BridgeAuth::Token);
    }

    #[test]
    fn create_and_close_hit_only_their_endpoints() {
        let mut transport = transport();
        FORGEJO
            .create_pull(
                &mut transport,
                "https://forge.example",
                "team/project",
                "bridge:branch",
                "main",
                "title",
                "body",
            )
            .unwrap_err();
        assert_eq!(transport.requests.len(), 1);
        assert_eq!(transport.requests[0].method, BridgeMethod::Post);
        assert_eq!(
            transport.requests[0].path,
            "/api/v1/repos/team/project/pulls"
        );
        assert_eq!(
            transport.requests[0].body.as_ref().unwrap()["head"],
            "bridge:branch"
        );
        FORGEJO
            .close_pull(
                &mut transport,
                "https://forge.example",
                "team/project",
                20,
                "closed body",
            )
            .unwrap();
        assert_eq!(transport.requests[1].method, BridgeMethod::Patch);
        assert_eq!(
            transport.requests[1].body.as_ref().unwrap()["state"],
            "closed"
        );
    }

    #[test]
    fn missing_pull_fields_fail_closed() {
        let mut transport = transport();
        transport.responses.insert(
            "/api/v1/repos/team/project/pulls/20".into(),
            json!({"number":20,"state":"open"}),
        );
        assert!(FORGEJO
            .pull(&mut transport, "https://forge.example", "team/project", 20)
            .is_err());
    }

    #[test]
    fn git_dest_points_at_the_import_fork_with_owner_user() {
        let dest = FORGEJO.git_dest("https://forge.example", "bridge/project");
        assert_eq!(dest.url, "https://forge.example/bridge/project.git");
        assert_eq!(dest.token_env, Some("CFRG_BRIDGE_FORGEJO_TOKEN"));
        assert_eq!(dest.username, "bridge");
    }
}

/// Authorization scope for exactly one Forgejo deployment: the validated
/// origin and primary repository. Only requests to that origin, with this
/// forge's credential, reach the network — reads under `/api/v1/`, and
/// exactly the two import mutations with numeric pull identities.
struct ForgejoPolicy {
    origin: String,
    primary: String,
}

fn numeric_idiom(segment: &str) -> Option<u64> {
    if segment.is_empty() || !segment.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    segment.parse::<u64>().ok().filter(|n| *n > 0)
}

impl MutationPolicy for ForgejoPolicy {
    fn authorize(&self, request: &BridgeRequest) -> Result<()> {
        if request.origin != self.origin {
            return Err("Forgejo scope rejects foreign origin".into());
        }
        if request.token_env != "CFRG_BRIDGE_FORGEJO_TOKEN" {
            return Err("Forgejo scope rejects foreign credential".into());
        }
        cfrg::bridge::reject_path_tricks(&request.path)?;
        let pulls = format!("/api/v1/repos/{}/pulls", self.primary);
        match request.method {
            BridgeMethod::Get => {
                if request.path.starts_with("/api/v1/") {
                    Ok(())
                } else {
                    Err("Forgejo scope refuses reads outside /api/v1/".into())
                }
            }
            BridgeMethod::Post => {
                if request.path == pulls {
                    Ok(())
                } else {
                    Err("Forgejo scope allows pull creation on the primary only".into())
                }
            }
            BridgeMethod::Patch => {
                let rest = request
                    .path
                    .strip_prefix(&format!("{pulls}/"))
                    .ok_or_else(|| {
                        "Forgejo scope allows pull closes on the primary only".to_owned()
                    })?;
                match numeric_idiom(rest) {
                    Some(_) => Ok(()),
                    None => Err("Forgejo scope refuses this PATCH".into()),
                }
            }
            BridgeMethod::Put => Err("Forgejo scope never PUTs".into()),
        }
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use cfrg::bridge::{BridgeAuth, BridgeMethod, BridgeRequest, MutationPolicy};

    fn policy() -> Box<dyn MutationPolicy> {
        FORGEJO.policy("https://forge.example", "team/project")
    }

    fn request(method: BridgeMethod, path: &str) -> BridgeRequest {
        request_with(
            "https://forge.example",
            "CFRG_BRIDGE_FORGEJO_TOKEN",
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
            auth: BridgeAuth::Token,
            method,
            path: path.into(),
            body: None,
        }
    }

    #[test]
    fn exact_import_mutations_pass() {
        let policy = policy();
        assert!(policy
            .authorize(&request(
                BridgeMethod::Post,
                "/api/v1/repos/team/project/pulls"
            ))
            .is_ok());
        assert!(policy
            .authorize(&request(
                BridgeMethod::Patch,
                "/api/v1/repos/team/project/pulls/20"
            ))
            .is_ok());
        assert!(policy
            .authorize(&request(BridgeMethod::Get, "/api/v1/repos/team/project"))
            .is_ok());
    }

    #[test]
    fn cross_origin_credential_mismatch_and_path_tricks_fail() {
        let policy = policy();
        assert!(policy
            .authorize(&request_with(
                "https://evil.example",
                "CFRG_BRIDGE_FORGEJO_TOKEN",
                BridgeMethod::Get,
                "/api/v1/repos/team/project"
            ))
            .is_err());
        assert!(policy
            .authorize(&request_with(
                "https://forge.example",
                "CFRG_BRIDGE_GITLAB_TOKEN",
                BridgeMethod::Get,
                "/api/v1/repos/team/project"
            ))
            .is_err());
        assert!(policy
            .authorize(&request(BridgeMethod::Get, "/api/v2/repos/team/project"))
            .is_err());
        for path in [
            "/api/v1/repos/team/project/pulls/../20",
            "/api/v1//repos/team/project",
            "/api/v1/repos/team/project/pulls/abc",
            "/api/v1/repos/other/project/pulls",
            "/api/v1/repos/other/project/pulls/20",
            "/api/v1/repos/team/project/pulls/20/merge",
        ] {
            // POST shape for the mutation cases, GET otherwise: every
            // combination outside the allowlist must fail.
            let method = if path.ends_with("/pulls") {
                BridgeMethod::Post
            } else if path.contains("/pulls/") {
                BridgeMethod::Patch
            } else {
                BridgeMethod::Get
            };
            assert!(policy.authorize(&request(method, path)).is_err(), "{path}");
        }
        // PUT never writes on Forgejo; DELETE is not a bridge verb at all.
        assert!(policy
            .authorize(&request(
                BridgeMethod::Put,
                "/api/v1/repos/team/project/pulls/20"
            ))
            .is_err());
    }
}

#[cfg(test)]
mod status_body_tests {
    use super::*;

    #[test]
    fn bodies_carry_context_target_and_description_per_state() {
        for (state, word) in [
            (State::Pending, "pending"),
            (State::Success, "success"),
            (State::Failure, "failure"),
        ] {
            let body = ForgejoStatus.status_body("ccid/verify", "https://ci.example/1", state);
            assert_eq!(body["state"], word);
            assert_eq!(body["context"], "ccid/verify");
            assert_eq!(body["target_url"], "https://ci.example/1");
            assert_eq!(body["description"], format!("ccid/verify: {word}"));
        }
    }
}
