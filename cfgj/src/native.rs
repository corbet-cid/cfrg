//! Forgejo owns the replication data plane; these are its typed API operations.
use cfrg::{
    native::{
        component,
        http::{expect, Auth, Request, Transport},
        path, Destination, Endpoint, Repository,
    },
    Result,
};
use serde_json::{json, Value};

/// A repository of the source as the copy sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub id: u64,
    /// `owner/repo`, lowercase: the name the copy carries.
    pub path: String,
    pub default_branch: String,
    pub archived: bool,
    pub has_wiki: bool,
}

/// Every repository the token sees, in id order. The listing is the declaration of
/// what the copy covers, so a repository nobody wrote down is covered all the same.
pub fn list_repositories(io: &mut dyn Transport, endpoint: &Endpoint) -> Result<Vec<Listed>> {
    let mut all = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for page in 1..=100 {
        let value = expect(
            io.send(Request {
                endpoint: endpoint.clone(),
                auth: Auth::Token,
                method: "GET",
                path: format!("/api/v1/repos/search?limit=50&page={page}&sort=id&order=asc"),
                body: None,
                scope: format!("{}/api", endpoint.origin),
                creation: false,
            })?,
            &[200],
        )?;
        let data = value["data"]
            .as_array()
            .ok_or("Malformed Forgejo repository search")?;
        if data.is_empty() {
            return Ok(all);
        }
        for item in data {
            let id = item["id"].as_u64().ok_or("Missing repository id")?;
            if !seen.insert(id) {
                return Err("Repeated repository page".into());
            }
            let path = item["full_name"]
                .as_str()
                .ok_or("Missing repository name")?
                .to_ascii_lowercase();
            cfrg::native::path(&path, false)?;
            all.push(Listed {
                id,
                path,
                default_branch: item["default_branch"]
                    .as_str()
                    .ok_or("Missing default branch")?
                    .to_string(),
                archived: item["archived"] == true,
                has_wiki: item["has_wiki"] == true,
            });
        }
    }
    Err("Forgejo repository pagination bound exceeded".into())
}

/// A repository of the source with the settings a copy must carry (R42).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Primary {
    pub id: u64,
    /// `owner/repo`, lowercase: the name a copy carries.
    pub path: String,
    /// A pull mirror of somebody else's project (a contributed project, R11): not ours to
    /// replicate, so it has no copies to keep in line.
    pub contributed: bool,
    /// The primary's issue tracker is on.
    pub issues: bool,
    /// The wiki unit is on (pages may still be absent: see [`wiki_exists`]).
    pub has_wiki: bool,
    pub settings: cfrg::native::settings::Primary,
}

/// Every repository the token sees with the settings R42 compares. One read per fifty.
pub fn list_primaries(io: &mut dyn Transport, endpoint: &Endpoint) -> Result<Vec<Primary>> {
    let mut all = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for page in 1..=100 {
        let value = expect(
            io.send(Request {
                endpoint: endpoint.clone(),
                auth: Auth::Token,
                method: "GET",
                path: format!("/api/v1/repos/search?limit=50&page={page}&sort=id&order=asc"),
                body: None,
                scope: format!("{}/api", endpoint.origin),
                creation: false,
            })?,
            &[200],
        )?;
        let data = value["data"]
            .as_array()
            .ok_or("Malformed Forgejo repository search")?;
        if data.is_empty() {
            return Ok(all);
        }
        for item in data {
            let id = item["id"].as_u64().ok_or("Missing repository id")?;
            if !seen.insert(id) {
                return Err("Repeated repository page".into());
            }
            let path = item["full_name"]
                .as_str()
                .ok_or("Missing repository name")?
                .to_ascii_lowercase();
            cfrg::native::path(&path, false)?;
            let text = |key: &str| item[key].as_str().unwrap_or_default().to_string();
            all.push(Primary {
                id,
                path,
                contributed: item["mirror"] == true,
                issues: item["has_issues"] == true,
                has_wiki: item["has_wiki"] == true,
                settings: cfrg::native::settings::Primary {
                    private: item["private"] == true,
                    description: text("description"),
                    homepage: text("website"),
                    topics: item["topics"]
                        .as_array()
                        .map(|rows| {
                            rows.iter()
                                .filter_map(|t| t.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    default_branch: text("default_branch"),
                    archived: item["archived"] == true,
                },
            });
        }
    }
    Err("Forgejo repository pagination bound exceeded".into())
}

/// The commit a branch of the primary points at, or `None` when it has no such branch.
pub fn branch_head(
    io: &mut dyn Transport,
    endpoint: &Endpoint,
    path: &str,
    branch: &str,
) -> Result<Option<String>> {
    cfrg::native::path(path, false)?;
    let response = io.send(Request {
        endpoint: endpoint.clone(),
        auth: Auth::Token,
        method: "GET",
        path: format!(
            "/api/v1/repos/{path}/branches/{}",
            cfrg::native::encode(branch)
        ),
        body: None,
        scope: format!("{}/api", endpoint.origin),
        creation: false,
    })?;
    if response.status == 404 {
        return Ok(None);
    }
    Ok(expect(response, &[200])?["commit"]["id"]
        .as_str()
        .map(String::from))
}

/// The members of the primary's projects: every non-owner team of every organisation (its
/// repositories are all the organisation's or the ones it lists) and every repository
/// collaborator. Organisation owners are not members here: they own the copies' side too.
pub fn list_members(
    io: &mut dyn Transport,
    endpoint: &Endpoint,
    paths: &[String],
) -> Result<Vec<cfrg::copy_access::Member>> {
    let get = |io: &mut dyn Transport, path: String| -> Result<Option<Value>> {
        let response = io.send(Request {
            endpoint: endpoint.clone(),
            auth: Auth::Token,
            method: "GET",
            path,
            body: None,
            scope: format!("{}/api", endpoint.origin),
            creation: false,
        })?;
        if response.status == 404 {
            return Ok(None);
        }
        Ok(Some(expect(response, &[200])?))
    };
    let mut by_login: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        std::collections::BTreeMap::new();
    let orgs: std::collections::BTreeSet<&str> =
        paths.iter().filter_map(|p| p.split('/').next()).collect();
    for org in orgs {
        // A personal namespace is no organisation: 404, no teams.
        let Some(teams) = get(io, format!("/api/v1/orgs/{org}/teams"))? else {
            continue;
        };
        for team in teams.as_array().into_iter().flatten() {
            if team["permission"] == "owner" {
                continue;
            }
            let id = team["id"].as_u64().ok_or("Missing team id")?;
            let covered: Vec<String> = if team["includes_all_repositories"] == true {
                paths
                    .iter()
                    .filter(|p| p.split('/').next() == Some(org))
                    .cloned()
                    .collect()
            } else {
                get(io, format!("/api/v1/teams/{id}/repos"))?
                    .as_ref()
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r["full_name"].as_str().map(str::to_ascii_lowercase))
                    .collect()
            };
            let members = get(io, format!("/api/v1/teams/{id}/members"))?;
            for member in members
                .as_ref()
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(login) = member["login"].as_str() {
                    by_login
                        .entry(login.to_string())
                        .or_default()
                        .extend(covered.iter().cloned());
                }
            }
        }
    }
    for path in paths {
        cfrg::native::path(path, false)?;
        let collaborators = get(io, format!("/api/v1/repos/{path}/collaborators"))?;
        for who in collaborators
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(login) = who["login"].as_str() {
                by_login
                    .entry(login.to_string())
                    .or_default()
                    .insert(path.clone());
            }
        }
    }
    Ok(by_login
        .into_iter()
        .map(|(login, repos)| cfrg::copy_access::Member { login, repos })
        .collect())
}

/// Whether the repository has a wiki to copy: the pages list answers 404 while the
/// wiki git repository does not exist, however the wiki unit is set.
pub fn wiki_exists(io: &mut dyn Transport, endpoint: &Endpoint, path: &str) -> Result<bool> {
    cfrg::native::path(path, false)?;
    let response = io.send(Request {
        endpoint: endpoint.clone(),
        auth: Auth::Token,
        method: "GET",
        path: format!("/api/v1/repos/{path}/wiki/pages?limit=1"),
        body: None,
        scope: format!("{}/api", endpoint.origin),
        creation: false,
    })?;
    if response.status == 404 {
        return Ok(false);
    }
    Ok(expect(response, &[200])?
        .as_array()
        .is_some_and(|pages| !pages.is_empty()))
}

pub struct Mirrors<'a> {
    pub endpoint: &'a Endpoint,
    pub repository: &'a Repository,
}

impl Mirrors<'_> {
    fn call(
        &self,
        io: &mut dyn Transport,
        method: &'static str,
        suffix: &str,
        body: Option<Value>,
        statuses: &[u16],
    ) -> Result<Value> {
        path(&self.repository.path, false)?;
        expect(
            io.send(Request {
                endpoint: self.endpoint.clone(),
                auth: Auth::Token,
                method,
                path: format!("/api/v1/repos/{}{}", self.repository.path, suffix),
                body,
                scope: format!("{}/api", self.endpoint.origin),
                creation: false,
            })?,
            statuses,
        )
    }
    /// Look the primary up by its immutable id and return its CURRENT
    /// `<org>/<repo>`. A repository renamed or moved on Forgejo keeps its id, so
    /// a stale declared path is a finding (`source-renamed`), not an error.
    pub fn verify_source(&self, io: &mut dyn Transport) -> Result<String> {
        let value = expect(
            io.send(Request {
                endpoint: self.endpoint.clone(),
                auth: Auth::Token,
                method: "GET",
                path: format!("/api/v1/repositories/{}", self.repository.source_id),
                body: None,
                scope: format!("{}/api", self.endpoint.origin),
                creation: false,
            })?,
            &[200],
        )?;
        if value["id"].as_u64() != Some(self.repository.source_id)
            || value["private"] != self.repository.private
            || value["default_branch"] != self.repository.default_branch
            || value["mirror"] != false
        {
            return Err(
                "Forgejo source identity, privacy, primary status or default branch mismatch"
                    .into(),
            );
        }
        let current = value["full_name"]
            .as_str()
            .ok_or("Missing Forgejo repository name")?;
        path(current, false)?;
        Ok(current.to_string())
    }
    /// The commit a branch of the primary points at (`None` when it is gone).
    pub fn head(&self, io: &mut dyn Transport, branch: &str) -> Result<Option<String>> {
        let branch = branch
            .split('/')
            .map(cfrg::native::encode)
            .collect::<Vec<_>>()
            .join("/");
        let response = io.send(Request {
            endpoint: self.endpoint.clone(),
            auth: Auth::Token,
            method: "GET",
            path: format!("/api/v1/repos/{}/branches/{branch}", self.repository.path),
            body: None,
            scope: format!("{}/api", self.endpoint.origin),
            creation: false,
        })?;
        if response.status == 404 {
            return Ok(None);
        }
        Ok(expect(response, &[200])?["commit"]["id"]
            .as_str()
            .map(String::from))
    }
    pub fn list(&self, io: &mut dyn Transport) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        for page in 1..=20 {
            let value = self.call(
                io,
                "GET",
                &format!("/push_mirrors?limit=50&page={page}"),
                None,
                &[200],
            )?;
            let values = value.as_array().ok_or("Malformed Forgejo mirror list")?;
            if values.is_empty() {
                return Ok(result);
            }
            for value in values {
                let name = value["remote_name"]
                    .as_str()
                    .ok_or("Missing mirror remote name")?;
                component(name)?;
                if result.iter().any(|old: &Value| old["remote_name"] == name) {
                    return Err("Repeated mirror page".into());
                }
                result.push(value.clone());
            }
        }
        Err("Forgejo mirror pagination bound exceeded".into())
    }
    pub fn create(
        &self,
        io: &mut dyn Transport,
        destination: &Destination,
        remote_address: &str,
        password: &str,
    ) -> Result<Value> {
        let mut body = json!({
            "remote_address": remote_address, "remote_username": match destination.provider {
                cfrg::native::Provider::Gitlab => "oauth2",
                cfrg::native::Provider::Bitbucket => "x-bitbucket-api-token-auth",
                // A token over HTTPS: the account that owns it is the user name.
                cfrg::native::Provider::Github => destination.mirror_user.as_str(),
            },
            "remote_password": password, "interval": format!("{}s", destination.interval_seconds),
            "sync_on_commit": destination.sync_on_commit, "branch_filter": destination.branch_filter,
        });
        if destination.use_ssh {
            let fields = body.as_object_mut().ok_or("Invalid mirror body")?;
            fields.remove("remote_username");
            fields.remove("remote_password");
            fields.insert("use_ssh".into(), json!(true));
        }
        self.call(io, "POST", "/push_mirrors", Some(body), &[200, 201])
    }
    /// An SSH push mirror to a plain git host: Forgejo generates the key pair and
    /// reports the public half; nothing else is passed.
    pub fn create_ssh(
        &self,
        io: &mut dyn Transport,
        remote_address: &str,
        interval_seconds: u64,
    ) -> Result<Value> {
        let body = json!({
            "remote_address": remote_address, "use_ssh": true,
            "interval": format!("{interval_seconds}s"),
            "sync_on_commit": true, "branch_filter": "",
        });
        self.call(io, "POST", "/push_mirrors", Some(body), &[200, 201])
    }
    /// Forgejo's API enqueues ALL mirrors on this repository, not one remote.
    /// The reconciler must verify that every listed mirror is authorized first.
    pub fn sync_now(&self, io: &mut dyn Transport) -> Result<()> {
        self.call(io, "POST", "/push_mirrors-sync", None, &[200])?;
        Ok(())
    }
    pub fn delete(&self, io: &mut dyn Transport, remote_name: &str) -> Result<()> {
        component(remote_name)?;
        self.call(
            io,
            "DELETE",
            &format!("/push_mirrors/{remote_name}"),
            None,
            &[204],
        )?;
        Ok(())
    }
}

/// Status deliberately excludes remote error text and URLs (can contain secrets).
pub fn status(mirror: &Value) -> Value {
    json!({"remote_name":mirror["remote_name"], "last_update":mirror["last_update"],
        "has_error":mirror["last_error"].as_str().is_none_or(|s| !s.is_empty()),
        "interval":mirror["interval"], "sync_on_commit":mirror["sync_on_commit"],
        "branch_filter":mirror["branch_filter"]})
}

pub fn matches(mirror: &Value, destination: &Destination, url: &str) -> bool {
    address_matches(mirror, url)
        && destination.use_ssh == mirror["public_key"].as_str().is_some_and(|s| !s.is_empty())
        && mirror["sync_on_commit"] == destination.sync_on_commit
        && mirror["branch_filter"].as_str().unwrap_or("") == destination.branch_filter
        && duration_seconds(mirror["interval"].as_str().unwrap_or(""))
            == Some(destination.interval_seconds)
}

pub fn address_matches(mirror: &Value, url: &str) -> bool {
    let address = mirror["remote_address"].as_str().unwrap_or("");
    if url.starts_with("ssh://git@") {
        // Forgejo sanitizes userinfo from the API's SSH remote address.
        return address == url || address == url.replacen("ssh://git@", "ssh://", 1);
    }
    // Forgejo may return a sanitized credential-bearing authority. Compare only
    // the declared HTTPS host/path, never log the provider's returned address.
    let clean = address.strip_prefix("https://").map(|s| {
        let (authority, path) = s.split_once('/').unwrap_or((s, ""));
        format!(
            "https://{}/{}",
            authority.rsplit('@').next().unwrap_or(""),
            path
        )
    });
    clean.as_deref() == Some(url)
}

/// What the Actions unit of a contributed repository on the source forge is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionsState {
    /// The repository does not exist (yet): nothing runs.
    Absent,
    /// Actions are off.
    Off,
    /// Actions are ON and were left so (a plan, or a read-only pass): the repository's own workflows would run.
    On,
    /// Actions were on and were switched off by this call.
    Disabled,
}

impl ActionsState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Off => "off",
            Self::On => "on",
            Self::Disabled => "disabled",
        }
    }
}

/// Keep Forgejo Actions off for one contributed repository (operating model R10, R11). A fork carries the upstream's
/// workflows (Forgejo reads `.github` when there is no `.forgejo`), so with Actions on, their CI would run on our
/// runners. Reads the repository; with `write`, switches the unit off and reads the result back.
pub fn contributed_actions(
    io: &mut dyn Transport,
    endpoint: &Endpoint,
    path: &str,
    write: bool,
) -> Result<ActionsState> {
    cfrg::native::path(path, false)?;
    let request = |method: &'static str, body: Option<Value>| Request {
        endpoint: endpoint.clone(),
        auth: Auth::Token,
        method,
        path: format!("/api/v1/repos/{path}"),
        body,
        scope: format!("{}/api", endpoint.origin),
        creation: false,
    };
    let enabled = |value: &Value| -> Result<bool> {
        value["has_actions"]
            .as_bool()
            .ok_or_else(|| "Forgejo response misses boolean has_actions".into())
    };
    let response = io.send(request("GET", None))?;
    if response.status == 404 {
        return Ok(ActionsState::Absent);
    }
    if !enabled(&expect(response, &[200])?)? {
        return Ok(ActionsState::Off);
    }
    if !write {
        return Ok(ActionsState::On);
    }
    let edited = expect(
        io.send(request("PATCH", Some(json!({"has_actions": false}))))?,
        &[200],
    )?;
    if enabled(&edited)? {
        return Err(format!("Forgejo kept Actions on for {path}").into());
    }
    Ok(ActionsState::Disabled)
}

/// Forgejo reports an interval as a Go duration (`10m0s`, `1h0m0s`).
pub fn duration_seconds(value: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut number = String::new();
    for c in value.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let factor = match c {
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
        total = total.checked_add(number.parse::<u64>().ok()?.checked_mul(factor)?)?;
        number.clear();
    }
    if number.is_empty() && !value.is_empty() {
        Some(total)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_never_reports_remote_errors_or_credentials() {
        let value = status(&json!({"remote_address":"https://user:secret@host/repo", "last_error":"secret", "remote_name":"remote_mirror_abc"})).to_string();
        assert!(!value.contains("secret"));
        assert!(!value.contains("remote_address"));
        assert_eq!(duration_seconds("1h0m0s"), Some(3600));
    }

    #[test]
    fn sanitized_ssh_userinfo_does_not_change_destination_identity() {
        let mirror = json!({"remote_address":"ssh://gitlab.example/team/project.git"});
        assert!(address_matches(
            &mirror,
            "ssh://git@gitlab.example/team/project.git"
        ));
        assert!(!address_matches(
            &mirror,
            "ssh://git@other.example/team/project.git"
        ));
        assert!(!address_matches(
            &mirror,
            "ssh://git@gitlab.example/team/other.git"
        ));
    }

    fn github_destination(extra: Value) -> Destination {
        let mut value = json!({"provider":"github","endpoint":{"origin":"https://api.github.com","token_env":"G"},"path":"org/repo","namespace":"org","repository_id":"7","mirror_user":"u","password_env":"G","interval_seconds":3600});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_destination_pushes_on_commit_unless_it_says_otherwise() {
        let url = "https://github.com/org/repo.git";
        let mirror = |on_commit: bool| json!({"remote_address": url, "interval": "1h0m0s", "sync_on_commit": on_commit, "branch_filter": ""});
        let default = github_destination(json!({}));
        assert!(default.sync_on_commit);
        assert!(matches(&mirror(true), &default, url));
        assert!(
            !matches(&mirror(false), &default, url),
            "a mirror that does not push on commit drifts from the default"
        );
        let paced = github_destination(json!({"sync_on_commit": false}));
        assert!(!paced.sync_on_commit);
        assert!(matches(&mirror(false), &paced, url));
        assert!(
            !matches(&mirror(true), &paced, url),
            "a mirror that pushes on every commit drifts from a paced destination"
        );
    }

    struct SshCreate;
    impl Transport for SshCreate {
        fn send(&mut self, request: Request) -> Result<cfrg::native::http::Response> {
            assert_eq!(request.path, "/api/v1/repos/team/project/push_mirrors");
            assert_eq!(request.method, "POST");
            let body = request.body.unwrap();
            assert_eq!(body["use_ssh"], true);
            assert_eq!(body["sync_on_commit"], true);
            assert_eq!(body["interval"], "3600s");
            assert!(body.get("remote_password").is_none());
            assert!(body.get("remote_username").is_none());
            Ok(cfrg::native::http::Response {
                status: 200,
                body: json!({"remote_name":"remote_mirror_test"}),
            })
        }
    }
    #[test]
    fn ssh_creation_passes_no_http_credentials_and_accepts_native_200() {
        let repo: Repository = serde_json::from_value(json!({"path":"team/project","source_id":1,"private":true,"default_branch":"main","content":"native-git","destinations":[]})).unwrap();
        let dest: Destination = serde_json::from_value(json!({"provider":"gitlab","endpoint":{"origin":"https://gitlab.example","token_env":"TOKEN"},"path":"team/project","namespace":"2","mirror_user":"9","password_env":"TOKEN","use_ssh":true,"interval_seconds":3600})).unwrap();
        let endpoint = Endpoint {
            origin: "https://forge.example".into(),
            token_env: "TOKEN".into(),
        };
        let value = Mirrors {
            endpoint: &endpoint,
            repository: &repo,
        }
        .create(
            &mut SshCreate,
            &dest,
            "ssh://git@gitlab.example/team/project.git",
            "must-not-leak",
        )
        .unwrap();
        assert_eq!(value["remote_name"], "remote_mirror_test");
    }
    struct Found(u64);
    impl Transport for Found {
        fn send(&mut self, request: Request) -> Result<cfrg::native::http::Response> {
            assert_eq!(request.method, "GET");
            assert_eq!(request.path, "/api/v1/repositories/7");
            Ok(cfrg::native::http::Response {
                status: 200,
                body: json!({"id":self.0,"full_name":"team/renamed","private":true,"default_branch":"main","mirror":false}),
            })
        }
    }
    #[test]
    fn the_primary_is_found_by_its_id_under_the_name_it_has_now() {
        let repo: Repository = serde_json::from_value(json!({"path":"team/project","source_id":7,"private":true,"default_branch":"main","content":"native-git","destinations":[]})).unwrap();
        let endpoint = Endpoint {
            origin: "https://forge.example".into(),
            token_env: "TOKEN".into(),
        };
        let api = Mirrors {
            endpoint: &endpoint,
            repository: &repo,
        };
        assert_eq!(api.verify_source(&mut Found(7)).unwrap(), "team/renamed");
        assert!(api.verify_source(&mut Found(8)).is_err());
    }

    struct Branch(u16);
    impl Transport for Branch {
        fn send(&mut self, request: Request) -> Result<cfrg::native::http::Response> {
            assert_eq!(request.method, "GET");
            assert_eq!(request.path, "/api/v1/repos/team/project/branches/ci/topic");
            Ok(cfrg::native::http::Response {
                status: self.0,
                body: json!({"name":"ci/topic","commit":{"id":"abc"}}),
            })
        }
    }
    #[test]
    fn the_head_of_a_branch_is_its_commit_or_nothing_when_it_is_gone() {
        let repo: Repository = serde_json::from_value(json!({"path":"team/project","source_id":7,"private":true,"default_branch":"main","content":"native-git","destinations":[]})).unwrap();
        let endpoint = Endpoint {
            origin: "https://forge.example".into(),
            token_env: "TOKEN".into(),
        };
        let api = Mirrors {
            endpoint: &endpoint,
            repository: &repo,
        };
        assert_eq!(
            api.head(&mut Branch(200), "ci/topic").unwrap().as_deref(),
            Some("abc")
        );
        assert_eq!(api.head(&mut Branch(404), "ci/topic").unwrap(), None);
    }

    struct Script {
        replies: Vec<(u16, Value)>,
        seen: Vec<(String, String, Option<Value>)>,
    }
    impl Transport for Script {
        fn send(&mut self, request: Request) -> Result<cfrg::native::http::Response> {
            self.seen.push((
                request.method.to_owned(),
                request.path.clone(),
                request.body.clone(),
            ));
            assert!(
                matches!(request.auth, Auth::Token) && !request.creation,
                "an Actions switch is an ordinary token call"
            );
            let (status, body) = self.replies.remove(0);
            Ok(cfrg::native::http::Response { status, body })
        }
    }
    fn script(replies: Vec<(u16, Value)>) -> Script {
        Script {
            replies,
            seen: Vec::new(),
        }
    }
    fn forge() -> Endpoint {
        Endpoint {
            origin: "https://forge.example".into(),
            token_env: "TOKEN".into(),
        }
    }

    #[test]
    fn a_contributed_repository_that_does_not_exist_runs_nothing() {
        let mut io = script(vec![(404, json!({}))]);
        let state = contributed_actions(&mut io, &forge(), "me/their-project", true).unwrap();
        assert_eq!(state, ActionsState::Absent);
        assert_eq!(io.seen.len(), 1);
        assert_eq!(io.seen[0].0, "GET");
    }

    #[test]
    fn actions_that_are_off_are_left_alone_even_when_writing() {
        let mut io = script(vec![(200, json!({"has_actions": false}))]);
        let state = contributed_actions(&mut io, &forge(), "me/their-project", true).unwrap();
        assert_eq!(state, ActionsState::Off);
        assert_eq!(
            io.seen.len(),
            1,
            "no write for a switch that is already off"
        );
    }

    #[test]
    fn a_plan_reports_actions_that_are_on_and_changes_nothing() {
        let mut io = script(vec![(200, json!({"has_actions": true}))]);
        let state = contributed_actions(&mut io, &forge(), "me/their-project", false).unwrap();
        assert_eq!(state, ActionsState::On);
        assert_eq!(io.seen.len(), 1);
        assert_eq!(io.seen[0].0, "GET");
    }

    #[test]
    fn actions_that_are_on_are_switched_off_and_read_back() {
        let mut io = script(vec![
            (200, json!({"has_actions": true})),
            (200, json!({"has_actions": false})),
        ]);
        let state = contributed_actions(&mut io, &forge(), "me/their-project", true).unwrap();
        assert_eq!(state, ActionsState::Disabled);
        assert_eq!(
            io.seen,
            vec![
                ("GET".into(), "/api/v1/repos/me/their-project".into(), None),
                (
                    "PATCH".into(),
                    "/api/v1/repos/me/their-project".into(),
                    Some(json!({"has_actions": false}))
                ),
            ],
            "the only change is the Actions switch"
        );
    }

    #[test]
    fn a_switch_that_forgejo_does_not_keep_is_an_error() {
        let mut io = script(vec![
            (200, json!({"has_actions": true})),
            (200, json!({"has_actions": true})),
        ]);
        let error = contributed_actions(&mut io, &forge(), "me/their-project", true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("kept Actions on"), "{error}");
    }

    #[test]
    fn an_answer_without_the_switch_is_an_error_not_a_guess() {
        let mut io = script(vec![(200, json!({"full_name": "me/their-project"}))]);
        assert!(contributed_actions(&mut io, &forge(), "me/their-project", true).is_err());
        let mut io = script(vec![(500, json!({}))]);
        assert!(contributed_actions(&mut io, &forge(), "me/their-project", true).is_err());
    }

    #[test]
    fn a_path_that_is_not_owner_and_repository_sends_nothing() {
        for path in ["../x", "me", "me/a/b", "me/their project", ""] {
            let mut io = script(vec![]);
            assert!(
                contributed_actions(&mut io, &forge(), path, true).is_err(),
                "{path}"
            );
            assert!(io.seen.is_empty(), "{path}");
        }
    }
}
