//! Forgejo landing and release, native first.
//!
//! Verified live on Forgejo 15.0.9 (see `docs/land.md`):
//! * `POST pulls/{n}/merge` with `Do=fast-forward-only` and
//!   `head_commit_id` merges exactly that head or fails (409 when the head
//!   moved, 405 while mergeability is recomputed or the required status is
//!   missing, 500 when the base moved and a fast-forward is impossible).
//! * `merge_when_checks_succeed` answers 201 and fires only on the NEXT
//!   success status of the head: it never merges a head that is already
//!   green, never survives a rebase, and silently never fires for a stale
//!   head. cfrg schedules it so the common case lands without any follower
//!   and fills the rest.
//! * `POST pulls/{n}/update?style=rebase` rebases the branch server-side
//!   (200, or 409 on a conflict).
//! * Branch protection with a required status context gates PR merges only;
//!   direct pushes by a user who may push stay possible (soft enforcement).
//! * Generic packages are immutable per version (`PUT` 201, then 409) and
//!   download anonymously from public owners; the files listing carries the
//!   SHA-256 the forge computed.
use cfrg::{
    land::{branch_name, commit_id, Capability, Entry, LandTarget, Merge, Support, MARKER},
    native::{
        encode,
        http::{expect, Auth, Request, Response, Transport},
        path, Endpoint,
    },
    observe::Observe,
    release::{Artifact, ReleaseTarget},
    status::State,
    Result,
};
use serde_json::{json, Value};

pub const LAND: Capability = Capability {
    support: Support::NativeFill,
    note: "native: pull request, merge-when-checks-succeed, fast-forward-only merge, server-side rebase, status-gated branch protection; cfrg fills: one queue per repository, rebase and retest when the base moved, merging a head that is already green",
};

pub const OBSERVE: Capability = Capability {
    support: Support::Native,
    note: "native: branch head and combined commit status of the repository",
};

pub const RELEASE: Capability = Capability {
    support: Support::Native,
    note: "native: generic package registry, immutable per version, anonymous download from public owners, forge-computed SHA-256",
};

pub struct Land<'a> {
    endpoint: &'a Endpoint,
    repository: &'a str,
    io: &'a mut dyn Transport,
    default_branch: Option<String>,
}

impl<'a> Land<'a> {
    pub fn new(
        endpoint: &'a Endpoint,
        repository: &'a str,
        io: &'a mut dyn Transport,
    ) -> Result<Self> {
        path(repository, false)?;
        Ok(Self {
            endpoint,
            repository,
            io,
            default_branch: None,
        })
    }

    fn scope(&self) -> String {
        format!("{}/api", self.endpoint.origin)
    }

    fn call(
        &mut self,
        method: &'static str,
        suffix: &str,
        body: Option<Value>,
    ) -> Result<Response> {
        let request = Request {
            endpoint: self.endpoint.clone(),
            auth: Auth::Token,
            method,
            path: format!("/api/v1/repos/{}{}", self.repository, suffix),
            body,
            scope: self.scope(),
            creation: false,
        };
        self.io.send(request)
    }

    fn get(&mut self, suffix: &str) -> Result<Value> {
        let response = self.call("GET", suffix, None)?;
        expect(response, &[200])
    }

    fn default_branch(&mut self) -> Result<String> {
        if let Some(name) = &self.default_branch {
            return Ok(name.clone());
        }
        let repo = self.get("")?;
        let name = repo["default_branch"]
            .as_str()
            .ok_or("Missing default branch")?
            .to_owned();
        branch_name(&name)?;
        self.default_branch = Some(name.clone());
        Ok(name)
    }

    /// Open pull requests, oldest first, bounded.
    fn pulls(&mut self) -> Result<Vec<Value>> {
        let mut all = Vec::new();
        for page in 1..=10 {
            let value = self.get(&format!(
                "/pulls?state=open&sort=oldest&limit=50&page={page}"
            ))?;
            let items = value.as_array().ok_or("Malformed pull request list")?;
            all.extend(items.iter().cloned());
            if items.len() < 50 {
                break;
            }
        }
        Ok(all)
    }

    fn own_branch(&self, pull: &Value, default: &str) -> bool {
        pull["base"]["ref"] == default && pull["head"]["repo"]["full_name"] == self.repository
    }

    /// Read a failed write back: the outcome is only known from the forge.
    fn read_back(&mut self, number: u64) -> Result<Value> {
        let pull = self.get(&format!("/pulls/{number}"))?;
        let scope = self.scope();
        self.io.resolve(&scope)?;
        Ok(pull)
    }
}

impl Observe for Land<'_> {
    fn branch_head(&mut self, branch: &str) -> Result<Option<String>> {
        branch_name(branch)?;
        let response = self.call("GET", &format!("/branches/{}", encode(branch)), None)?;
        if response.status == 404 {
            return Ok(None);
        }
        let found = expect(response, &[200])?;
        let head = found["commit"]["id"]
            .as_str()
            .ok_or("Missing branch head")?;
        commit_id(head)?;
        Ok(Some(head.to_owned()))
    }

    fn commit_statuses(&mut self, commit: &str) -> Result<Vec<(String, State)>> {
        LandTarget::statuses(self, commit)
    }
}

pub fn entry(pull: &Value) -> Result<Entry> {
    let number = pull["number"]
        .as_u64()
        .ok_or("Missing pull request number")?;
    let branch = pull["head"]["ref"].as_str().ok_or("Missing head branch")?;
    branch_name(branch)?;
    let head = pull["head"]["sha"].as_str().ok_or("Missing head commit")?;
    commit_id(head)?;
    let base_tip = pull["base"]["sha"].as_str().ok_or("Missing base commit")?;
    commit_id(base_tip)?;
    Ok(Entry {
        number,
        branch: branch.into(),
        head: head.into(),
        base_tip: base_tip.into(),
    })
}

pub fn state(status: &str) -> State {
    match status {
        "success" => State::Success,
        "failure" | "error" => State::Failure,
        // pending, warning and anything unknown never count as green.
        _ => State::Pending,
    }
}

impl LandTarget for Land<'_> {
    fn queue(&mut self) -> Result<Vec<Entry>> {
        let default = self.default_branch()?;
        let pulls = self.pulls()?;
        Ok(pulls
            .iter()
            .filter(|p| self.own_branch(p, &default))
            .filter(|p| p["body"].as_str().is_some_and(|b| b.contains(MARKER)))
            .filter_map(|p| entry(p).ok())
            .collect())
    }

    fn enqueue(&mut self, branch: &str) -> Result<Entry> {
        branch_name(branch)?;
        let default = self.default_branch()?;
        if branch == default {
            return Err("The default branch cannot be landed onto itself".into());
        }
        let pulls = self.pulls()?;
        let existing = pulls
            .iter()
            .find(|p| self.own_branch(p, &default) && p["head"]["ref"] == branch);
        if let Some(pull) = existing {
            let found = entry(pull)?;
            let body = pull["body"].as_str().unwrap_or("");
            if !body.contains(MARKER) {
                let adopted = format!("{MARKER}\n{body}");
                let response = self.call(
                    "PATCH",
                    &format!("/pulls/{}", found.number),
                    Some(json!({ "body": adopted })),
                )?;
                expect(response, &[200, 201])?;
            }
            return Ok(found);
        }
        let ahead = self.call("GET", &format!("/compare/{default}...{branch}"), None)?;
        if ahead.status == 404 {
            return Err("The branch is not on the forge; push it first".into());
        }
        let ahead = expect(ahead, &[200])?;
        if ahead["total_commits"].as_u64() == Some(0) {
            return Err(
                "Nothing to land: the branch is already contained in the default branch".into(),
            );
        }
        let response = self.call(
            "POST",
            "/pulls",
            Some(json!({
                "title": format!("land {branch}"),
                "head": branch,
                "base": default,
                "body": format!("{MARKER}\nQueued by `cfrg land`: merges only the exact green head, fast-forward-only."),
            })),
        )?;
        match response.status {
            201 => entry(&response.body),
            404 => Err("The branch is not on the forge; push it first".into()),
            409 | 422 => {
                Err("Nothing to land: the branch has no commit beyond the default branch".into())
            }
            other => Err(format!("Forgejo refused the pull request (HTTP {other})").into()),
        }
    }

    fn contains_tip(&mut self, entry: &Entry) -> Result<bool> {
        let default = self.default_branch()?;
        let tip = self.get(&format!("/branches/{default}"))?;
        let tip = tip["commit"]["id"].as_str().ok_or("Missing branch tip")?;
        commit_id(tip)?;
        // Commits the default branch has that the head lacks; none means the
        // head already contains the tip and a fast-forward is possible.
        let compare = self.get(&format!("/compare/{}...{}", entry.head, tip))?;
        Ok(compare["total_commits"].as_u64() == Some(0))
    }

    fn statuses(&mut self, sha: &str) -> Result<Vec<(String, State)>> {
        commit_id(sha)?;
        let combined = self.get(&format!("/commits/{sha}/status?limit=100&page=1"))?;
        Ok(combined["statuses"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|s| {
                        Some((
                            s["context"].as_str()?.to_owned(),
                            state(s["status"].as_str()?),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn schedule(&mut self, entry: &Entry) -> Result<()> {
        let response = self.call(
            "POST",
            &format!("/pulls/{}/merge", entry.number),
            Some(json!({
                "Do": "fast-forward-only",
                "merge_when_checks_succeed": true,
                "head_commit_id": entry.head,
                "delete_branch_after_merge": false,
            })),
        )?;
        // 201 scheduled; 409 already scheduled or head moved; a refusal is
        // harmless because the follower merges a green head itself.
        if ![201, 405, 409, 422].contains(&response.status) {
            return Err(format!(
                "Forgejo refused to schedule the merge (HTTP {})",
                response.status
            )
            .into());
        }
        Ok(())
    }

    fn rebase(&mut self, entry: &Entry) -> Result<Option<Entry>> {
        let outcome = self.call(
            "POST",
            &format!("/pulls/{}/update?style=rebase", entry.number),
            None,
        );
        match outcome {
            Ok(response) => match response.status {
                200 => {}
                409 => return Ok(None),
                other => {
                    return Err(format!("Forgejo refused the rebase (HTTP {other})").into());
                }
            },
            Err(error) => {
                // Outcome unknown: the branch head tells whether it happened.
                let pull = self.read_back(entry.number)?;
                let fresh = self::entry(&pull)?;
                if fresh.head == entry.head {
                    return Err(error);
                }
                return Ok(Some(fresh));
            }
        }
        let pull = self.get(&format!("/pulls/{}", entry.number))?;
        Ok(Some(self::entry(&pull)?))
    }

    fn merge(&mut self, entry: &Entry) -> Result<Merge> {
        let outcome = self.call(
            "POST",
            &format!("/pulls/{}/merge", entry.number),
            Some(json!({"Do": "fast-forward-only", "head_commit_id": entry.head})),
        );
        match outcome {
            Ok(response) => match response.status {
                200 => Ok(Merge::Merged),
                405 => Ok(Merge::NotReady),
                409 => Ok(Merge::HeadMoved),
                other => Err(format!("Forgejo refused the merge (HTTP {other})").into()),
            },
            Err(error) => {
                // A server error can follow a committed merge, or a base that
                // moved under us; only the forge's own state can tell.
                let pull = self.read_back(entry.number)?;
                if pull["merged"] == true {
                    Ok(Merge::Merged)
                } else {
                    eprintln!(
                        "{}",
                        json!({"event":"merge-error","number":entry.number,"error":error.to_string()})
                    );
                    Ok(Merge::NotReady)
                }
            }
        }
    }

    fn merged(&mut self, number: u64) -> Result<Option<String>> {
        let pull = self.get(&format!("/pulls/{number}"))?;
        if pull["merged"] != true {
            return Ok(None);
        }
        let sha = pull["merge_commit_sha"]
            .as_str()
            .or_else(|| pull["head"]["sha"].as_str())
            .ok_or("Missing merged commit")?;
        commit_id(sha)?;
        Ok(Some(sha.into()))
    }

    fn abandon(&mut self, entry: &Entry, reason: &str) -> Result<()> {
        let comment = self.call(
            "POST",
            &format!("/issues/{}/comments", entry.number),
            Some(json!({ "body": reason })),
        )?;
        expect(comment, &[201])?;
        let closed = self.call(
            "PATCH",
            &format!("/pulls/{}", entry.number),
            Some(json!({ "state": "closed" })),
        )?;
        expect(closed, &[200, 201])?;
        Ok(())
    }
}

/// Reconcile the default branch's protection: merges require the CI status.
/// Soft enforcement: an existing rule keeps its push settings untouched, a
/// new rule lets every writer push exactly as before. Without `apply` this
/// only reports what it would do.
pub fn protect(
    io: &mut dyn Transport,
    endpoint: &Endpoint,
    repository: &str,
    contexts: &[String],
    apply: bool,
) -> Result<Value> {
    path(repository, false)?;
    let mut call = |method: &'static str, suffix: &str, body: Option<Value>| {
        io.send(Request {
            endpoint: endpoint.clone(),
            auth: Auth::Token,
            method,
            path: format!("/api/v1/repos/{repository}{suffix}"),
            body,
            scope: format!("{}/api", endpoint.origin),
            creation: false,
        })
    };
    let repo = expect(call("GET", "", None)?, &[200])?;
    let rule = repo["default_branch"]
        .as_str()
        .ok_or("Missing default branch")?
        .to_owned();
    branch_name(&rule)?;
    let current = call("GET", &format!("/branch_protections/{rule}"), None)?;
    let report = |state: &str| json!({"repository": repository, "rule": rule, "state": state});
    let wanted = json!(contexts);
    if current.status == 404 {
        if !apply {
            return Ok(report("create-planned"));
        }
        let body = json!({
            "rule_name": rule, "branch_name": rule,
            "enable_push": true, "enable_push_whitelist": false,
            "enable_merge_whitelist": false, "required_approvals": 0,
            "enable_status_check": true, "status_check_contexts": wanted,
        });
        expect(
            call("POST", "/branch_protections", Some(body))?,
            &[200, 201],
        )?;
        return Ok(report("created"));
    }
    let existing = expect(current, &[200])?;
    let mut have: Vec<&str> = existing["status_check_contexts"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut want: Vec<&str> = contexts.iter().map(String::as_str).collect();
    have.sort_unstable();
    want.sort_unstable();
    if existing["enable_status_check"] == true && have == want {
        return Ok(report("ok"));
    }
    if !apply {
        return Ok(report("update-planned"));
    }
    let body = json!({"enable_status_check": true, "status_check_contexts": wanted});
    expect(
        call("PATCH", &format!("/branch_protections/{rule}"), Some(body))?,
        &[200, 201],
    )?;
    Ok(report("updated"))
}

/// Forgejo's generic package registry.
pub struct Packages;
pub static PACKAGES: Packages = Packages;

impl ReleaseTarget for Packages {
    fn file_path(&self, a: &Artifact) -> String {
        format!(
            "/api/packages/{}/generic/{}/{}/{}",
            a.owner, a.package, a.version, a.file
        )
    }
    fn listing_path(&self, a: &Artifact) -> String {
        format!(
            "/api/v1/packages/{}/generic/{}/{}/files",
            a.owner, a.package, a.version
        )
    }
    fn auth(&self) -> Auth {
        Auth::Token
    }
    fn stored_sha256(&self, listing: &Value, a: &Artifact) -> Option<String> {
        listing
            .as_array()?
            .iter()
            .find(|f| f["name"] == a.file.as_str())
            .and_then(|f| f["sha256"].as_str().map(String::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Scripted forge: each call must match the next expectation.
    struct Script(VecDeque<(&'static str, String, u16, Value)>);
    impl Transport for Script {
        fn send(&mut self, request: Request) -> Result<Response> {
            let (method, prefix, status, body) = self.0.pop_front().expect("unexpected call");
            assert_eq!(request.method, method, "{}", request.path);
            assert!(
                request.path.contains(&prefix),
                "{} lacks {prefix}",
                request.path
            );
            Ok(Response { status, body })
        }
    }

    fn endpoint() -> Endpoint {
        Endpoint {
            origin: "https://forge.example".into(),
            token_env: "TOKEN".into(),
        }
    }

    fn pull(number: u64, branch: &str, head: char, body: &str) -> Value {
        json!({"number": number, "body": body,
            "head": {"ref": branch, "sha": head.to_string().repeat(40), "repo": {"full_name": "o/r"}},
            "base": {"ref": "main", "sha": "b".repeat(40)}})
    }

    fn script(calls: Vec<(&'static str, &str, u16, Value)>) -> Script {
        Script(
            calls
                .into_iter()
                .map(|(m, p, s, b)| (m, p.to_string(), s, b))
                .collect(),
        )
    }

    #[test]
    fn queue_keeps_only_marked_default_branch_entries_of_this_repository() {
        let mut fork = pull(3, "fork", 'c', MARKER);
        fork["head"]["repo"]["full_name"] = json!("someone/r");
        let mut other = pull(4, "other-base", 'd', MARKER);
        other["base"]["ref"] = json!("dev");
        let mut io = script(vec![
            (
                "GET",
                "/api/v1/repos/o/r",
                200,
                json!({"default_branch": "main"}),
            ),
            (
                "GET",
                "/pulls?state=open&sort=oldest",
                200,
                json!([
                    pull(1, "ci/one", 'a', &format!("{MARKER} x")),
                    pull(2, "human", 'e', "no marker"),
                    fork,
                    other,
                ]),
            ),
        ]);
        let endpoint = endpoint();
        let queue = Land::new(&endpoint, "o/r", &mut io)
            .unwrap()
            .queue()
            .unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].branch, "ci/one");
        assert_eq!(queue[0].head, "a".repeat(40));
    }

    #[test]
    fn enqueue_creates_the_pull_request_or_adopts_an_unmarked_one() {
        let endpoint = endpoint();
        let mut io = script(vec![
            (
                "GET",
                "/api/v1/repos/o/r",
                200,
                json!({"default_branch": "main"}),
            ),
            ("GET", "/pulls?state=open", 200, json!([])),
            (
                "GET",
                "/compare/main...ci/new",
                200,
                json!({"total_commits": 1}),
            ),
            ("POST", "/pulls", 201, pull(7, "ci/new", 'a', MARKER)),
        ]);
        let made = Land::new(&endpoint, "o/r", &mut io)
            .unwrap()
            .enqueue("ci/new")
            .unwrap();
        assert_eq!(made.number, 7);
        let mut io = script(vec![
            (
                "GET",
                "/api/v1/repos/o/r",
                200,
                json!({"default_branch": "main"}),
            ),
            (
                "GET",
                "/pulls?state=open",
                200,
                json!([pull(8, "ci/old", 'a', "hand made")]),
            ),
            ("PATCH", "/pulls/8", 201, Value::Null),
        ]);
        let adopted = Land::new(&endpoint, "o/r", &mut io)
            .unwrap()
            .enqueue("ci/old")
            .unwrap();
        assert_eq!(adopted.number, 8);
        let mut io = script(vec![
            (
                "GET",
                "/api/v1/repos/o/r",
                200,
                json!({"default_branch": "main"}),
            ),
            ("GET", "/pulls?state=open", 200, json!([])),
            (
                "GET",
                "/compare/main...ci/done",
                200,
                json!({"total_commits": 0}),
            ),
        ]);
        let done = Land::new(&endpoint, "o/r", &mut io)
            .unwrap()
            .enqueue("ci/done");
        assert!(done.unwrap_err().to_string().contains("already contained"));
        let mut io = script(vec![]);
        assert!(Land::new(&endpoint, "o/r", &mut io)
            .unwrap()
            .enqueue("bad branch")
            .is_err());
    }

    #[test]
    fn contains_tip_compares_the_head_with_the_fresh_default_tip() {
        let endpoint = endpoint();
        let entry = entry(&pull(1, "ci/one", 'a', MARKER)).unwrap();
        let mut io = script(vec![
            (
                "GET",
                "/api/v1/repos/o/r",
                200,
                json!({"default_branch": "main"}),
            ),
            (
                "GET",
                "/branches/main",
                200,
                json!({"commit": {"id": "f".repeat(40)}}),
            ),
            (
                "GET",
                &format!("/compare/{}...{}", "a".repeat(40), "f".repeat(40)),
                200,
                json!({"total_commits": 2}),
            ),
        ]);
        assert!(!Land::new(&endpoint, "o/r", &mut io)
            .unwrap()
            .contains_tip(&entry)
            .unwrap());
    }

    #[test]
    fn statuses_map_to_states_and_only_success_is_green() {
        let endpoint = endpoint();
        let mut io = script(vec![(
            "GET",
            "/status",
            200,
            json!({"statuses": [
                {"context": "ci/a", "status": "success"},
                {"context": "ci/b", "status": "warning"},
                {"context": "ci/c", "status": "error"},
            ]}),
        )]);
        let got = Land::new(&endpoint, "o/r", &mut io)
            .unwrap()
            .statuses(&"a".repeat(40))
            .unwrap();
        assert_eq!(
            got,
            [
                ("ci/a".to_string(), State::Success),
                ("ci/b".to_string(), State::Pending),
                ("ci/c".to_string(), State::Failure),
            ]
        );
        let mut io = script(vec![(
            "GET",
            "/status",
            200,
            json!({"state": "", "total_count": 0}),
        )]);
        assert!(Land::new(&endpoint, "o/r", &mut io)
            .unwrap()
            .statuses(&"a".repeat(40))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn merge_and_rebase_map_forgejo_answers() {
        let endpoint = endpoint();
        let entry = entry(&pull(1, "ci/one", 'a', MARKER)).unwrap();
        for (status, want) in [
            (200, Merge::Merged),
            (405, Merge::NotReady),
            (409, Merge::HeadMoved),
        ] {
            let mut io = script(vec![("POST", "/pulls/1/merge", status, Value::Null)]);
            assert_eq!(
                Land::new(&endpoint, "o/r", &mut io)
                    .unwrap()
                    .merge(&entry)
                    .unwrap(),
                want
            );
        }
        let mut io = script(vec![(
            "POST",
            "/pulls/1/update?style=rebase",
            409,
            Value::Null,
        )]);
        assert!(Land::new(&endpoint, "o/r", &mut io)
            .unwrap()
            .rebase(&entry)
            .unwrap()
            .is_none());
        let mut io = script(vec![
            ("POST", "/pulls/1/update?style=rebase", 200, Value::Null),
            ("GET", "/pulls/1", 200, pull(1, "ci/one", 'c', MARKER)),
        ]);
        let fresh = Land::new(&endpoint, "o/r", &mut io)
            .unwrap()
            .rebase(&entry)
            .unwrap()
            .unwrap();
        assert_eq!(fresh.head, "c".repeat(40));
    }

    #[test]
    fn a_merged_pull_request_reports_the_landed_commit() {
        let endpoint = endpoint();
        let mut io = script(vec![
            (
                "GET",
                "/pulls/4",
                200,
                json!({"merged": true, "merge_commit_sha": "a".repeat(40)}),
            ),
            ("GET", "/pulls/5", 200, json!({"merged": false})),
        ]);
        let mut land = Land::new(&endpoint, "o/r", &mut io).unwrap();
        assert_eq!(land.merged(4).unwrap(), Some("a".repeat(40)));
        assert_eq!(land.merged(5).unwrap(), None);
    }

    #[test]
    fn protection_is_created_soft_and_updated_without_touching_pushes() {
        let endpoint = endpoint();
        let contexts = vec!["ci/*".to_string()];
        let mut io = script(vec![
            (
                "GET",
                "/api/v1/repos/o/r",
                200,
                json!({"default_branch": "main"}),
            ),
            ("GET", "/branch_protections/main", 404, Value::Null),
        ]);
        let plan = protect(&mut io, &endpoint, "o/r", &contexts, false).unwrap();
        assert_eq!(plan["state"], "create-planned");
        let mut io = script(vec![
            (
                "GET",
                "/api/v1/repos/o/r",
                200,
                json!({"default_branch": "main"}),
            ),
            (
                "GET",
                "/branch_protections/main",
                200,
                json!({"enable_status_check": true, "status_check_contexts": ["ci/*"]}),
            ),
        ]);
        assert_eq!(
            protect(&mut io, &endpoint, "o/r", &contexts, true).unwrap()["state"],
            "ok"
        );
        let mut io = script(vec![
            (
                "GET",
                "/api/v1/repos/o/r",
                200,
                json!({"default_branch": "main"}),
            ),
            (
                "GET",
                "/branch_protections/main",
                200,
                json!({"enable_status_check": false, "status_check_contexts": [], "enable_push_whitelist": true}),
            ),
            ("PATCH", "/branch_protections/main", 200, Value::Null),
        ]);
        assert_eq!(
            protect(&mut io, &endpoint, "o/r", &contexts, true).unwrap()["state"],
            "updated"
        );
    }

    #[test]
    fn package_paths_are_stable_and_hash_comes_from_the_listing() {
        let artifact = Artifact {
            owner: "corbet-libs".into(),
            package: "cfrg".into(),
            version: "a".repeat(40),
            file: "cfrg-x86_64-unknown-linux-gnu".into(),
            sha256: String::new(),
            size: 0,
        };
        assert_eq!(
            PACKAGES.file_path(&artifact),
            format!(
                "/api/packages/corbet-libs/generic/cfrg/{}/cfrg-x86_64-unknown-linux-gnu",
                "a".repeat(40)
            )
        );
        let listing = json!([{"name": "other", "sha256": "1"}, {"name": "cfrg-x86_64-unknown-linux-gnu", "sha256": "ab"}]);
        assert_eq!(
            PACKAGES.stored_sha256(&listing, &artifact).as_deref(),
            Some("ab")
        );
    }
}
