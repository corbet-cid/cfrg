//! Mocked: a scripted transport, no network.
use super::*;
use crate::native::{http::Request, plan_move, Move};
use std::collections::VecDeque;

type Scripted = (&'static str, String, VecDeque<(u16, Value)>);

/// Answers by (method, path); records every request. An unscripted request fails the test.
struct Fake {
    script: Vec<Scripted>,
    seen: Vec<(String, String, Option<Value>, bool)>,
}

impl Fake {
    fn new() -> Self {
        Self {
            script: Vec::new(),
            seen: Vec::new(),
        }
    }
    fn on(mut self, method: &'static str, path: &str, status: u16, body: Value) -> Self {
        match self
            .script
            .iter_mut()
            .find(|(m, p, _)| *m == method && p == path)
        {
            Some((_, _, queue)) => queue.push_back((status, body)),
            None => self
                .script
                .push((method, path.to_string(), VecDeque::from([(status, body)]))),
        }
        self
    }
    fn writes(&self) -> Vec<String> {
        self.seen
            .iter()
            .filter(|(m, ..)| m != "GET")
            .map(|(m, p, ..)| format!("{m} {p}"))
            .collect()
    }
}

impl Transport for Fake {
    fn send(&mut self, request: Request) -> Result<Response> {
        self.seen.push((
            request.method.to_string(),
            request.path.clone(),
            request.body.clone(),
            request.creation,
        ));
        let entry = self
            .script
            .iter_mut()
            .find(|(m, p, _)| *m == request.method && *p == request.path)
            .unwrap_or_else(|| panic!("unscripted request {} {}", request.method, request.path));
        let (status, body) = if entry.2.len() > 1 {
            entry.2.pop_front().unwrap()
        } else {
            entry.2.front().cloned().unwrap()
        };
        Ok(Response { status, body })
    }
}

fn dest(provider: &str, origin: &str, path: &str, id: &str) -> Destination {
    serde_json::from_value(json!({
        "provider": provider,
        "endpoint": {"origin": origin, "token_env": "T"},
        "path": path,
        "namespace": "CORE",
        "repository_id": id,
        "mirror_user": "m",
        "password_env": "P",
        "interval_seconds": 600,
        "hold": null,
        "remote_name": null,
    }))
    .unwrap()
}

fn gl() -> Destination {
    dest("gitlab", "https://gitlab.example", "new-group/app", "42")
}

fn gitlab_free(fake: Fake) -> Fake {
    fake.on("GET", "/api/v4/groups/new-group", 200, json!({"id": 7}))
        .on("GET", "/api/v4/projects/new-group%2Fapp", 404, json!({}))
}

#[test]
fn a_namespace_change_is_a_transfer_not_a_rename() {
    assert_eq!(plan_move("old/app", "new-group/app"), Move::Transfer);
}

#[test]
fn gitlab_plan_names_the_call_and_writes_nothing() {
    let mut io = gitlab_free(Fake::new());
    let outcome = gitlab(&mut io, &gl(), "old-group/app", false).unwrap();
    let Transfer::Planned { from, steps } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(from, "old-group/app");
    assert!(
        steps[0].contains("PUT /projects/42/transfer namespace=7"),
        "{steps:?}"
    );
    assert!(io.writes().is_empty());
}

#[test]
fn gitlab_apply_transfers_to_the_group_id_and_confirms_the_path() {
    let mut io = gitlab_free(Fake::new()).on(
        "PUT",
        "/api/v4/projects/42/transfer",
        200,
        json!({"path_with_namespace": "new-group/app"}),
    );
    let outcome = gitlab(&mut io, &gl(), "old-group/app", true).unwrap();
    assert!(matches!(outcome, Transfer::Done { .. }), "{outcome:?}");
    assert_eq!(io.writes(), ["PUT /api/v4/projects/42/transfer"]);
    assert_eq!(io.seen.last().unwrap().2, Some(json!({"namespace": 7})));
}

#[test]
fn gitlab_unconfirmed_transfer_is_an_error_not_a_success() {
    let mut io = gitlab_free(Fake::new()).on(
        "PUT",
        "/api/v4/projects/42/transfer",
        200,
        json!({"path_with_namespace": "somewhere/else"}),
    );
    assert!(gitlab(&mut io, &gl(), "old-group/app", true).is_err());
}

#[test]
fn gitlab_refusals_are_reported_blocked() {
    let mut io = gitlab_free(Fake::new()).on(
        "PUT",
        "/api/v4/projects/42/transfer",
        400,
        json!({"message": "Project cannot be transferred"}),
    );
    let outcome = gitlab(&mut io, &gl(), "old-group/app", true).unwrap();
    assert!(
        matches!(&outcome, Transfer::Blocked { reason, .. } if reason.contains("cannot be transferred")),
        "{outcome:?}"
    );
    let mut io = Fake::new().on("GET", "/api/v4/groups/new-group", 404, json!({}));
    assert!(matches!(
        gitlab(&mut io, &gl(), "old-group/app", true).unwrap(),
        Transfer::Blocked { .. }
    ));
    assert!(io.writes().is_empty());
}

#[test]
fn gitlab_name_taken_or_different_leaf_is_blocked_before_any_write() {
    let mut io = Fake::new()
        .on("GET", "/api/v4/groups/new-group", 200, json!({"id": 7}))
        .on(
            "GET",
            "/api/v4/projects/new-group%2Fapp",
            200,
            json!({"id": 99}),
        );
    assert!(matches!(
        gitlab(&mut io, &gl(), "old-group/app", true).unwrap(),
        Transfer::Blocked { .. }
    ));
    let mut io = Fake::new();
    assert!(matches!(
        gitlab(&mut io, &gl(), "old-group/other", true).unwrap(),
        Transfer::Blocked { .. }
    ));
    assert!(io.seen.is_empty() && io.writes().is_empty());
}

#[test]
fn gitlab_already_at_the_target_is_done_without_a_write() {
    let mut io = Fake::new()
        .on("GET", "/api/v4/groups/new-group", 200, json!({"id": 7}))
        .on(
            "GET",
            "/api/v4/projects/new-group%2Fapp",
            200,
            json!({"id": 42}),
        );
    assert!(matches!(
        gitlab(&mut io, &gl(), "old-group/app", true).unwrap(),
        Transfer::Done { .. }
    ));
    assert!(io.writes().is_empty());
}

fn gh() -> Destination {
    dest("github", "https://api.github.com", "new-org/app", "5")
}

fn github_free(fake: Fake) -> Fake {
    fake.on("GET", "/orgs/new-org", 200, json!({}))
        .on("GET", "/repos/new-org/app", 404, json!({}))
}

#[test]
fn github_plan_writes_nothing_and_says_it_is_gated() {
    let mut io = github_free(Fake::new());
    let outcome = github(&mut io, &gh(), "old-org/app", false, &Gate::default()).unwrap();
    let Transfer::Planned { steps, .. } = outcome else {
        panic!("{outcome:?}")
    };
    assert!(steps[0].contains("write gate"));
    assert!(io.writes().is_empty());
}

#[test]
fn github_a_closed_gate_blocks_before_any_request() {
    let mut io = Fake::new();
    let gate = Gate {
        refusal: Some("gate-kill-switch".into()),
    };
    let outcome = github(&mut io, &gh(), "old-org/app", true, &gate).unwrap();
    assert!(
        matches!(&outcome, Transfer::Blocked { reason, .. } if reason.contains("gate-kill-switch"))
    );
    assert!(io.seen.is_empty());
}

#[test]
fn github_apply_posts_once_as_a_creation_and_reads_back_by_id() {
    let mut io = github_free(Fake::new())
        .on("POST", "/repos/old-org/app/transfer", 202, json!({}))
        .on(
            "GET",
            "/repositories/5",
            200,
            json!({"full_name": "new-org/app"}),
        );
    let outcome = github(&mut io, &gh(), "old-org/app", true, &Gate::default()).unwrap();
    assert!(matches!(outcome, Transfer::Done { .. }), "{outcome:?}");
    assert_eq!(io.writes(), ["POST /repos/old-org/app/transfer"]);
    let post = io.seen.iter().find(|(m, ..)| m == "POST").unwrap();
    assert_eq!(post.2, Some(json!({"new_owner": "new-org"})));
    assert!(post.3, "paced as a creation");
}

#[test]
fn github_a_transfer_that_has_not_landed_is_pending_not_done() {
    let mut io = github_free(Fake::new())
        .on("POST", "/repos/old-org/app/transfer", 202, json!({}))
        .on(
            "GET",
            "/repositories/5",
            200,
            json!({"full_name": "old-org/app"}),
        );
    let outcome = github(&mut io, &gh(), "old-org/app", true, &Gate::default()).unwrap();
    assert!(matches!(outcome, Transfer::Pending { .. }), "{outcome:?}");
}

#[test]
fn github_missing_target_org_and_taken_names_are_blocked() {
    let mut io = Fake::new().on("GET", "/orgs/new-org", 404, json!({}));
    assert!(matches!(
        github(&mut io, &gh(), "old-org/app", true, &Gate::default()).unwrap(),
        Transfer::Blocked { .. }
    ));
    let mut io = Fake::new().on("GET", "/orgs/new-org", 200, json!({})).on(
        "GET",
        "/repos/new-org/app",
        200,
        json!({"id": 6}),
    );
    assert!(matches!(
        github(&mut io, &gh(), "old-org/app", true, &Gate::default()).unwrap(),
        Transfer::Blocked { .. }
    ));
    assert!(io.writes().is_empty());
}

fn bb() -> Destination {
    dest(
        "bitbucket",
        "https://api.bitbucket.org",
        "corbet-archv/corbet.ch",
        "{old}",
    )
}

fn mv() -> BitbucketMove<'static> {
    BitbucketMove {
        from: "corbet-web/corbet.ch",
        old_uuid: "{old}",
        project_key: "CORE",
        private: true,
    }
}

const NEW: &str = "/2.0/repositories/corbet-archv/corbet.ch";
const OLD: &str = "/2.0/repositories/corbet-web/corbet.ch";

fn refs(pairs: &[(&str, &str, &str)]) -> Value {
    json!({"values": pairs.iter().map(|(kind, name, hash)| json!({"type": kind, "name": name, "target": {"hash": hash}})).collect::<Vec<_>>()})
}

#[test]
fn bitbucket_plan_shows_the_four_steps_and_writes_nothing() {
    let mut io = Fake::new().on("GET", NEW, 404, json!({}));
    let outcome = bitbucket(&mut io, &bb(), &mv(), false).unwrap();
    let Transfer::Planned { steps, .. } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(steps.len(), 4);
    assert!(steps[0].contains("project CORE") && steps[0].contains("cfrg-transfer-from:{old}"));
    assert!(steps[3].contains("only after the compare matches"));
    assert!(io.writes().is_empty());
}

#[test]
fn bitbucket_apply_creates_the_marked_target_and_deletes_nothing() {
    let mut io =
        Fake::new()
            .on("GET", NEW, 404, json!({}))
            .on("POST", NEW, 200, json!({"uuid": "{new}"}));
    let outcome = bitbucket(&mut io, &bb(), &mv(), true).unwrap();
    assert!(
        matches!(&outcome, Transfer::Pending { new_repository_id: Some(id), .. } if id == "{new}"),
        "{outcome:?}"
    );
    assert_eq!(io.writes(), [format!("POST {NEW}")]);
    let body = io
        .seen
        .iter()
        .find(|(m, ..)| m == "POST")
        .unwrap()
        .2
        .clone()
        .unwrap();
    assert_eq!(body["project"]["key"], "CORE");
    assert_eq!(body["description"], "cfrg-transfer-from:{old}");
    assert_eq!(body["is_private"], true);
}

#[test]
fn bitbucket_a_foreign_repository_at_the_wanted_name_is_never_adopted_or_deleted() {
    let mut io = Fake::new().on(
        "GET",
        NEW,
        200,
        json!({"uuid": "{x}", "description": "mine"}),
    );
    let outcome = bitbucket(&mut io, &bb(), &mv(), true).unwrap();
    assert!(matches!(outcome, Transfer::Blocked { .. }));
    assert!(io.writes().is_empty());
}

fn marked() -> Value {
    json!({"uuid": "{new}", "description": "cfrg-transfer-from:{old}"})
}

#[test]
fn bitbucket_every_ref_must_match_before_the_old_copy_goes() {
    let same = refs(&[("branch", "main", "a1"), ("tag", "v1", "b2")]);
    // A differing branch, a missing tag, an extra branch: each alone keeps the old repository.
    for new in [
        refs(&[("branch", "main", "ZZ"), ("tag", "v1", "b2")]),
        refs(&[("branch", "main", "a1")]),
        refs(&[
            ("branch", "main", "a1"),
            ("tag", "v1", "b2"),
            ("branch", "extra", "c3"),
        ]),
    ] {
        let mut io = Fake::new()
            .on("GET", NEW, 200, marked())
            .on("GET", &format!("{OLD}/refs?pagelen=100"), 200, same.clone())
            .on("GET", &format!("{NEW}/refs?pagelen=100"), 200, new);
        let outcome = bitbucket(&mut io, &bb(), &mv(), true).unwrap();
        assert!(matches!(outcome, Transfer::Pending { .. }), "{outcome:?}");
        assert!(io.writes().is_empty(), "no delete without an exact match");
    }
}

#[test]
fn bitbucket_exact_match_deletes_the_old_repository_and_reports_the_new_id() {
    let same = refs(&[("branch", "main", "a1"), ("tag", "v1", "b2")]);
    let mut io = Fake::new()
        .on("GET", NEW, 200, marked())
        .on("GET", &format!("{OLD}/refs?pagelen=100"), 200, same.clone())
        .on("GET", &format!("{NEW}/refs?pagelen=100"), 200, same)
        .on("DELETE", OLD, 204, json!(null));
    let outcome = bitbucket(&mut io, &bb(), &mv(), true).unwrap();
    let Transfer::Done { detail, .. } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(detail["repository_id"], "{new}");
    assert_eq!(detail["refs_compared"], 2);
    assert_eq!(io.writes(), [format!("DELETE {OLD}")]);
}

#[test]
fn bitbucket_plan_with_a_matching_target_still_deletes_nothing() {
    let same = refs(&[("branch", "main", "a1")]);
    let mut io = Fake::new()
        .on("GET", NEW, 200, marked())
        .on("GET", &format!("{OLD}/refs?pagelen=100"), 200, same.clone())
        .on("GET", &format!("{NEW}/refs?pagelen=100"), 200, same);
    let outcome = bitbucket(&mut io, &bb(), &mv(), false).unwrap();
    assert!(matches!(outcome, Transfer::Planned { .. }));
    assert!(io.writes().is_empty());
}

#[test]
fn bitbucket_ref_listing_follows_pagination() {
    let page1 = json!({"values": [{"type":"branch","name":"main","target":{"hash":"a1"}}], "next": "https://api.bitbucket.org/2.0/repositories/corbet-web/corbet.ch/refs?pagelen=100&page=2"});
    let page2 = refs(&[("tag", "v1", "b2")]);
    let mut io = Fake::new()
        .on("GET", &format!("{OLD}/refs?pagelen=100"), 200, page1)
        .on("GET", &format!("{OLD}/refs?pagelen=100&page=2"), 200, page2);
    let all = bitbucket_refs(&mut io, &bb(), "corbet-web/corbet.ch").unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all["tag:v1"], "b2");
}

#[test]
fn compare_names_what_differs() {
    let old = BTreeMap::from([("branch:a".to_string(), "1".to_string())]);
    assert!(compare_refs(&old, &old).is_empty());
    assert_eq!(compare_refs(&old, &BTreeMap::new()).len(), 1);
}

#[test]
fn a_destination_with_another_name_than_the_old_one_is_not_moved_and_renamed_at_once() {
    let mut io = Fake::new();
    let mut other = bb();
    other.path = "corbet-archv/renamed".into();
    assert!(matches!(
        bitbucket(&mut io, &other, &mv(), true).unwrap(),
        Transfer::Blocked { .. }
    ));
    assert!(io.seen.is_empty());
}

#[test]
fn the_corbet_ch_dry_run_plan() {
    let mut io = Fake::new().on("GET", NEW, 404, json!({}));
    let outcome = bitbucket(&mut io, &bb(), &mv(), false).unwrap();
    let Transfer::Planned { from, steps } = outcome else {
        panic!()
    };
    let row = json!({"source": "corbet-web/corbet.ch", "destination": "corbet-archv/corbet.ch", "state": "transfer-planned", "from": from, "steps": steps});
    println!("{}", serde_json::to_string_pretty(&row).unwrap());
    assert_eq!(row["steps"].as_array().unwrap().len(), 4);
}
