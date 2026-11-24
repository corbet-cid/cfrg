//! Mocked: a scripted transport, no network.
use super::*;
use crate::native::{http::Request, plan_move, Move};
use std::collections::VecDeque;

type Scripted = (&'static str, String, VecDeque<(u16, Value)>);

/// Answers by (method, path); records every request. An unscripted request fails the test.
struct Fake {
    script: Vec<Scripted>,
    seen: Vec<(String, String, Option<Value>, bool)>,
    notes: Vec<String>,
}

impl Fake {
    fn new() -> Self {
        Self {
            script: Vec::new(),
            seen: Vec::new(),
            notes: Vec::new(),
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
        // Like the real transport: a 403 is an error that marks a refusal, other error bodies are never parsed.
        if status == 403 {
            return Err(failure(format!(
                "Native API HTTP 403; {REFUSED_MARK}; the run goes on"
            )));
        }
        let body = if status >= 400 { Value::Null } else { body };
        Ok(Response { status, body })
    }
    fn note(&mut self, kind: &str, key: &str, value: &str) -> Result<()> {
        self.notes.push(format!("{kind} {key} {value}"));
        Ok(())
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
        matches!(&outcome, Transfer::Blocked { reason, .. } if reason.contains("HTTP 400")),
        "{outcome:?}"
    );
    // A 403 reaches the move as the transport's refusal error and is a blocked move, not a failed run.
    let mut io = gitlab_free(Fake::new()).on("PUT", "/api/v4/projects/42/transfer", 403, json!({}));
    let outcome = gitlab(&mut io, &gl(), "old-group/app", true).unwrap();
    assert!(
        matches!(&outcome, Transfer::Blocked { reason, .. } if reason.contains("refused")),
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
    let outcome = github(
        &mut io,
        &gh(),
        "old-org/app",
        false,
        &Gate::default(),
        false,
    )
    .unwrap();
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
    let outcome = github(&mut io, &gh(), "old-org/app", true, &gate, false).unwrap();
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
    let outcome = github(&mut io, &gh(), "old-org/app", true, &Gate::default(), false).unwrap();
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
    let outcome = github(&mut io, &gh(), "old-org/app", true, &Gate::default(), false).unwrap();
    assert!(matches!(outcome, Transfer::Pending { .. }), "{outcome:?}");
}

#[test]
fn github_missing_target_org_and_taken_names_are_blocked() {
    let mut io = Fake::new().on("GET", "/orgs/new-org", 404, json!({}));
    assert!(matches!(
        github(&mut io, &gh(), "old-org/app", true, &Gate::default(), false).unwrap(),
        Transfer::Blocked { .. }
    ));
    let mut io = Fake::new().on("GET", "/orgs/new-org", 200, json!({})).on(
        "GET",
        "/repos/new-org/app",
        200,
        json!({"id": 6}),
    );
    assert!(matches!(
        github(&mut io, &gh(), "old-org/app", true, &Gate::default(), false).unwrap(),
        Transfer::Blocked { .. }
    ));
    assert!(io.writes().is_empty());
}

#[test]
fn github_the_post_is_noted_before_it_is_sent_and_never_repeated_when_requested() {
    let mut io = github_free(Fake::new())
        .on("POST", "/repos/old-org/app/transfer", 202, json!({}))
        .on(
            "GET",
            "/repositories/5",
            200,
            json!({"full_name": "old-org/app"}),
        );
    let outcome = github(&mut io, &gh(), "old-org/app", true, &Gate::default(), false).unwrap();
    assert!(matches!(outcome, Transfer::Pending { .. }));
    assert!(
        io.notes[0].starts_with("github-transfer 5 "),
        "{:?}",
        io.notes
    );
    // The next pass: requested earlier, read back only.
    let mut io = Fake::new().on(
        "GET",
        "/repositories/5",
        200,
        json!({"full_name": "old-org/app"}),
    );
    let outcome = github(&mut io, &gh(), "old-org/app", true, &Gate::default(), true).unwrap();
    assert!(matches!(outcome, Transfer::Pending { .. }));
    assert!(io.writes().is_empty());
    let mut io = Fake::new().on(
        "GET",
        "/repositories/5",
        200,
        json!({"full_name": "new-org/app"}),
    );
    let outcome = github(&mut io, &gh(), "old-org/app", true, &Gate::default(), true).unwrap();
    assert!(matches!(outcome, Transfer::Done { .. }));
    assert_eq!(io.notes, ["github-transfer-clear 5 "]);
}

#[test]
fn github_a_refused_post_clears_the_record_and_is_blocked() {
    let mut io = github_free(Fake::new()).on("POST", "/repos/old-org/app/transfer", 403, json!({}));
    let outcome = github(&mut io, &gh(), "old-org/app", true, &Gate::default(), false).unwrap();
    assert!(matches!(outcome, Transfer::Blocked { .. }), "{outcome:?}");
    assert_eq!(io.notes.len(), 2);
    assert!(io.notes[1].starts_with("github-transfer-clear"));
}

fn bb() -> Destination {
    dest(
        "bitbucket",
        "https://api.bitbucket.org",
        "corbet-archv/corbet.ch",
        "{old}",
    )
}

fn mv<'a>(allow_create: bool, known_new: Option<&'a str>) -> BitbucketMove<'a> {
    BitbucketMove {
        old_workspace: "corbet-web",
        old_uuid: "{old}",
        project_key: "CORE",
        private: true,
        allow_create,
        known_new,
    }
}

const NEW: &str = "/2.0/repositories/corbet-archv/corbet.ch";
const OLD: &str = "/2.0/repositories/corbet-web/%7Bold%7D";

fn old_repo() -> Value {
    json!({"uuid": "{old}", "full_name": "corbet-web/corbet.ch"})
}

fn refs(pairs: &[(&str, &str, &str)]) -> Value {
    json!({"values": pairs.iter().map(|(kind, name, hash)| json!({"type": kind, "name": name, "target": {"hash": hash}})).collect::<Vec<_>>()})
}

fn marked() -> Value {
    json!({"uuid": "{new}", "description": "cfrg-transfer-from:{old}"})
}

#[test]
fn bitbucket_plan_shows_the_four_steps_and_writes_nothing() {
    let mut io = Fake::new()
        .on("GET", NEW, 404, json!({}))
        .on("GET", OLD, 200, old_repo());
    let outcome = bitbucket(&mut io, &bb(), &mv(true, None), false).unwrap();
    let Transfer::Planned { steps, .. } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(steps.len(), 4);
    assert!(steps[0].contains("project CORE") && steps[0].contains("cfrg-transfer-from:{old}"));
    assert!(steps[3].contains("only after the compare matches"));
    assert!(io.writes().is_empty());
}

#[test]
fn bitbucket_apply_creates_the_marked_target_reads_it_back_and_deletes_nothing() {
    let mut io = Fake::new()
        .on("GET", NEW, 404, json!({}))
        .on("GET", NEW, 200, marked())
        .on("GET", OLD, 200, old_repo())
        .on("POST", NEW, 200, json!({"uuid": "{new}"}));
    let outcome = bitbucket(&mut io, &bb(), &mv(true, None), true).unwrap();
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
fn bitbucket_a_rename_operation_never_creates() {
    let mut io = Fake::new()
        .on("GET", NEW, 404, json!({}))
        .on("GET", OLD, 200, old_repo());
    let outcome = bitbucket(&mut io, &bb(), &mv(false, None), true).unwrap();
    assert!(
        matches!(&outcome, Transfer::Blocked { reason, .. } if reason.contains("never creates")),
        "{outcome:?}"
    );
    assert!(io.writes().is_empty());
}

#[test]
fn bitbucket_a_foreign_repository_at_the_wanted_name_is_never_adopted_or_deleted() {
    let mut io = Fake::new()
        .on(
            "GET",
            NEW,
            200,
            json!({"uuid": "{x}", "description": "mine"}),
        )
        .on("GET", OLD, 200, old_repo());
    let outcome = bitbucket(&mut io, &bb(), &mv(true, None), true).unwrap();
    assert!(matches!(outcome, Transfer::Blocked { .. }));
    assert!(io.writes().is_empty());
}

#[test]
fn bitbucket_the_recorded_uuid_accepts_a_target_whose_marker_was_rewritten() {
    let same = refs(&[("branch", "main", "a1")]);
    let rewritten = json!({"uuid": "{new}", "description": "a description settings wrote"});
    let mut io = Fake::new()
        .on("GET", NEW, 200, rewritten.clone())
        .on("GET", OLD, 200, old_repo())
        .on("GET", OLD, 200, old_repo())
        .on("GET", OLD, 404, json!({}))
        .on("GET", &format!("{OLD}/refs?pagelen=100"), 200, same.clone())
        .on("GET", &format!("{NEW}/refs?pagelen=100"), 200, same)
        .on("DELETE", OLD, 204, json!(null));
    let outcome = bitbucket(&mut io, &bb(), &mv(true, Some("{new}")), true).unwrap();
    assert!(matches!(outcome, Transfer::Done { .. }), "{outcome:?}");
    // Without the record the same target is somebody else's.
    let mut io = Fake::new()
        .on("GET", NEW, 200, rewritten)
        .on("GET", OLD, 200, old_repo());
    assert!(matches!(
        bitbucket(&mut io, &bb(), &mv(true, None), true).unwrap(),
        Transfer::Blocked { .. }
    ));
}

#[test]
fn bitbucket_every_ref_must_match_before_the_old_copy_goes() {
    let same = refs(&[("branch", "main", "a1"), ("tag", "v1", "b2")]);
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
            .on("GET", OLD, 200, old_repo())
            .on("GET", &format!("{OLD}/refs?pagelen=100"), 200, same.clone())
            .on("GET", &format!("{NEW}/refs?pagelen=100"), 200, new);
        let outcome = bitbucket(&mut io, &bb(), &mv(true, None), true).unwrap();
        assert!(matches!(outcome, Transfer::Pending { .. }), "{outcome:?}");
        assert!(io.writes().is_empty(), "no delete without an exact match");
    }
}

#[test]
fn bitbucket_an_empty_old_repository_is_never_deleted_on_an_empty_compare() {
    let none = refs(&[]);
    let mut io = Fake::new()
        .on("GET", NEW, 200, marked())
        .on("GET", OLD, 200, old_repo())
        .on("GET", &format!("{OLD}/refs?pagelen=100"), 200, none);
    let outcome = bitbucket(&mut io, &bb(), &mv(true, None), true).unwrap();
    assert!(
        matches!(&outcome, Transfer::Blocked { reason, .. } if reason.contains("no refs")),
        "{outcome:?}"
    );
    assert!(io.writes().is_empty());
}

#[test]
fn bitbucket_a_listing_without_values_is_an_error_not_an_empty_repository() {
    let mut io = Fake::new().on(
        "GET",
        &format!("{OLD}/refs?pagelen=100"),
        200,
        json!({"pagelen": 100}),
    );
    assert!(bitbucket_refs(&mut io, &bb(), "corbet-web/%7Bold%7D").is_err());
}

#[test]
fn bitbucket_exact_match_deletes_by_uuid_after_a_last_identity_check_and_reads_back() {
    let same = refs(&[("branch", "main", "a1"), ("tag", "v1", "b2")]);
    let mut io = Fake::new()
        .on("GET", NEW, 200, marked())
        .on("GET", OLD, 200, old_repo())
        .on("GET", OLD, 200, old_repo())
        .on("GET", OLD, 404, json!({}))
        .on("GET", &format!("{OLD}/refs?pagelen=100"), 200, same.clone())
        .on("GET", &format!("{NEW}/refs?pagelen=100"), 200, same)
        .on("DELETE", OLD, 204, json!(null));
    let outcome = bitbucket(&mut io, &bb(), &mv(true, None), true).unwrap();
    let Transfer::Done { detail, .. } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(detail["repository_id"], "{new}");
    assert_eq!(detail["refs_compared"], 2);
    assert_eq!(io.writes(), [format!("DELETE {OLD}")]);
}

#[test]
fn bitbucket_a_swapped_old_repository_is_not_deleted() {
    let same = refs(&[("branch", "main", "a1")]);
    let swapped = json!({"uuid": "{old}", "full_name": "corbet-web/other"});
    let mut io = Fake::new()
        .on("GET", NEW, 200, marked())
        .on("GET", OLD, 200, old_repo())
        .on("GET", OLD, 200, swapped)
        .on("GET", &format!("{OLD}/refs?pagelen=100"), 200, same.clone())
        .on("GET", &format!("{NEW}/refs?pagelen=100"), 200, same);
    assert!(bitbucket(&mut io, &bb(), &mv(true, None), true).is_err());
    assert!(io.writes().is_empty());
}

#[test]
fn bitbucket_plan_with_a_matching_target_still_deletes_nothing() {
    let same = refs(&[("branch", "main", "a1")]);
    let mut io = Fake::new()
        .on("GET", NEW, 200, marked())
        .on("GET", OLD, 200, old_repo())
        .on("GET", &format!("{OLD}/refs?pagelen=100"), 200, same.clone())
        .on("GET", &format!("{NEW}/refs?pagelen=100"), 200, same);
    let outcome = bitbucket(&mut io, &bb(), &mv(true, None), false).unwrap();
    assert!(matches!(outcome, Transfer::Planned { .. }));
    assert!(io.writes().is_empty());
}

#[test]
fn bitbucket_resumes_after_the_old_repository_is_gone() {
    // The delete happened, the placement still holds the old uuid: the marked successor completes the move.
    let mut io = Fake::new()
        .on("GET", NEW, 200, marked())
        .on("GET", OLD, 404, json!({}));
    let outcome = bitbucket(&mut io, &bb(), &mv(true, None), true).unwrap();
    let Transfer::Done { detail, .. } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(detail["repository_id"], "{new}");
    assert!(io.writes().is_empty());
    // No successor: refuse instead of guessing.
    let mut io = Fake::new()
        .on("GET", NEW, 404, json!({}))
        .on("GET", OLD, 404, json!({}));
    assert!(bitbucket(&mut io, &bb(), &mv(true, None), true).is_err());
}

#[test]
fn bitbucket_ref_listing_follows_pagination() {
    let page1 = json!({"values": [{"type":"branch","name":"main","target":{"hash":"a1"}}], "next": "https://api.bitbucket.org/2.0/repositories/corbet-web/%7Bold%7D/refs?pagelen=100&page=2"});
    let page2 = refs(&[("tag", "v1", "b2")]);
    let mut io = Fake::new()
        .on("GET", &format!("{OLD}/refs?pagelen=100"), 200, page1)
        .on("GET", &format!("{OLD}/refs?pagelen=100&page=2"), 200, page2);
    let all = bitbucket_refs(&mut io, &bb(), "corbet-web/%7Bold%7D").unwrap();
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
    let mut io = Fake::new()
        .on(
            "GET",
            NEW.replace("corbet.ch", "renamed").as_str(),
            404,
            json!({}),
        )
        .on("GET", OLD, 200, old_repo());
    let mut other = bb();
    other.path = "corbet-archv/renamed".into();
    assert!(matches!(
        bitbucket(&mut io, &other, &mv(true, None), true).unwrap(),
        Transfer::Blocked { .. }
    ));
    assert!(io.writes().is_empty());
}
