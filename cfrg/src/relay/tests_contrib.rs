//! End-to-end tests of the contribution flows (RL4, RL5) against fake forges: every path from a request event on
//! any forge to the sibling requests, the notice, the moved heads, the comments, and the closing.

use super::contrib::Contrib;
use super::fake_heads::{FakeGit, FakeHost, Shared, SharedNotices, World};
use super::heads::{Heads, ReqState};
use super::model::{Actor, Hint, HintAction, Kind};
use super::notice::ContributionMarker;
use super::tests::{alice, bob, FJ, GH, GL, H};
use super::Outcome;
use crate::model::Forge;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

const GH_PATH: &str = "acme-gh/app";
const GL_PATH: &str = "acme-gl/app";
const FJ_PATH: &str = "acme/app";

struct C {
    h: H,
    heads: Shared,
    notices: SharedNotices,
}

impl C {
    fn new() -> Self {
        let mut h = H::new();
        let heads: Shared = Rc::new(RefCell::new(World::default()));
        let mut hs = Heads::new(Box::new(FakeGit(heads.clone())));
        for (forge, handle, path, id) in [
            (Forge::Github, &h.gh, GH_PATH, GH),
            (Forge::Gitlab, &h.gl, GL_PATH, GL),
            (Forge::Forgejo, &h.fj, FJ_PATH, FJ),
        ] {
            hs.insert(Box::new(FakeHost {
                forge,
                w: heads.clone(),
                forge_world: Some((
                    handle.clone(),
                    BTreeMap::from([(path.to_owned(), id.to_owned())]),
                )),
            }));
        }
        let notices = SharedNotices::default();
        h.contrib = Some(Contrib {
            heads: hs,
            notices: Box::new(notices.clone()),
        });
        Self { h, heads, notices }
    }

    fn world(&self, forge: Forge) -> &super::fake::Handle {
        match forge {
            Forge::Github => &self.h.gh,
            Forge::Gitlab => &self.h.gl,
            Forge::Forgejo => &self.h.fj,
            Forge::Bitbucket => &self.h.bb,
        }
    }

    fn open(&mut self, forge: Forge, author: &Actor) -> u64 {
        let id = H::repo_id(forge);
        self.world(forge).borrow_mut().open_pull(
            id,
            author,
            "Fix the crash",
            "Fixes #7.",
            "feature",
            A,
            "main",
        )
    }

    fn hint(&self, forge: Forge, number: u64, actor: &Actor) -> Hint {
        Hint {
            forge,
            repo: H::repo_id(forge).into(),
            kind: Kind::Contribution,
            number,
            action: HintAction::Changed,
            actor: Some(actor.clone()),
        }
    }

    fn event(&mut self, forge: Forge, number: u64, actor: &Actor) -> super::Report {
        let hint = self.hint(forge, number, actor);
        self.h.push_hint(hint)
    }

    /// Everything a repeated, unchanged event must leave exactly as it is.
    fn writes(&self) -> (usize, usize, Vec<String>) {
        (
            self.heads.borrow().log.len(),
            self.notices.0.borrow().writes,
            self.h.mutations(),
        )
    }

    fn requests(&self, repo: &str) -> Vec<(u64, ReqState)> {
        self.heads
            .borrow()
            .requests
            .iter()
            .filter(|((r, _), _)| r == repo)
            .map(|((_, n), (_, _, s))| (*n, *s))
            .collect()
    }

    fn pull_state(&self, forge: Forge, number: u64) -> ReqState {
        self.world(forge).borrow().pulls[&(H::repo_id(forge).to_owned(), number)].state
    }

    fn notice_marker(&self) -> ContributionMarker {
        let n = self.notices.0.borrow();
        ContributionMarker::parse(&n.notices[0].2).expect("a marker on the last line")
    }
}

fn counted(c: &C) -> usize {
    c.heads.borrow().log.len()
}

// --- (a), (b): a request on any forge leads to credited siblings and one notice --------------------------

#[test]
fn a_github_pull_request_shows_on_gitlab_and_as_one_notice() {
    let mut c = C::new();
    let n = c.open(Forge::Github, &alice());
    let report = c.event(Forge::Github, n, &alice());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    // The sibling merge request on GitLab, from the relay fork, credited (R41).
    assert_eq!(c.requests(GL_PATH), vec![(1, ReqState::Open)]);
    assert_eq!(
        c.heads.borrow().branches[&(
            "acme-gl/cforge/app".to_owned(),
            "cforge/github-1".to_owned()
        )],
        A
    );
    let mr = c.h.gl.borrow_mut().issue(GL, 1);
    assert_eq!(mr.author.login, "relay-bot");
    assert!(
        mr.snap
            .body
            .starts_with("**alice** wrote this on GitHub: https://github.example/gh1/pull/1\n"),
        "{}",
        mr.snap.body
    );
    assert!(mr.snap.body.contains("Fixes "));
    // No second request on GitHub (it is the origin) and none on Bitbucket (R55).
    assert!(c.requests(GH_PATH).is_empty());
    assert_eq!(c.h.bb.borrow().requests(), 0);
    // One notice on the primary, marker last.
    assert_eq!(c.notices.0.borrow().notices.len(), 1);
    let m = c.notice_marker();
    assert_eq!(
        (m.origin.as_str(), m.reference.as_str(), m.sha.as_str()),
        ("https://github.example/gh1.git", "refs/pull/1/head", A)
    );
}

#[test]
fn a_gitlab_merge_request_shows_on_github_with_a_head_branch_in_the_copy() {
    let mut c = C::new();
    let n = c.open(Forge::Gitlab, &bob());
    let report = c.event(Forge::Gitlab, n, &bob());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    assert_eq!(c.requests(GH_PATH), vec![(1, ReqState::Open)]);
    assert_eq!(
        c.heads.borrow().branches[&(GH_PATH.to_owned(), "cforge/gitlab-1".to_owned())],
        A
    );
    assert_eq!(c.notice_marker().reference, "refs/merge-requests/1/head");
    let pr = c.h.gh.borrow_mut().issue(GH, 1);
    assert!(pr.snap.body.starts_with("**bob** wrote this on GitLab:"));
}

#[test]
fn a_forgejo_pull_request_from_a_fork_shows_on_both_copies_and_as_a_notice() {
    let mut c = C::new();
    let n = c.open(Forge::Forgejo, &alice());
    let report = c.event(Forge::Forgejo, n, &alice());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    assert_eq!(c.requests(GH_PATH).len(), 1);
    assert_eq!(c.requests(GL_PATH).len(), 1);
    assert_eq!(c.notices.0.borrow().notices.len(), 1);
    assert_eq!(c.notice_marker().reference, "refs/pull/1/head");
}

// --- idempotence ------------------------------------------------------------------------------------------

#[test]
fn a_repeated_event_changes_nothing() {
    let mut c = C::new();
    let n = c.open(Forge::Github, &alice());
    c.event(Forge::Github, n, &alice());
    let before = c.writes();
    for _ in 0..3 {
        let report = c.event(Forge::Github, n, &alice());
        assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
        assert_eq!(report.writes, 0);
    }
    assert_eq!(c.writes(), before);
}

#[test]
fn a_lost_sibling_is_repaired_by_a_reconcile_hint_only() {
    let mut c = C::new();
    let n = c.open(Forge::Github, &alice());
    c.event(Forge::Github, n, &alice());
    // Somebody deleted the relay branch on the GitLab fork.
    c.heads.borrow_mut().branches.clear();
    let before = counted(&c);
    c.event(Forge::Github, n, &alice());
    assert_eq!(counted(&c), before, "an ordinary repeat is a no-op");
    let mut hint = c.hint(Forge::Github, n, &alice());
    hint.action = HintAction::Reconcile;
    c.h.push_hint(hint);
    assert!(c.heads.borrow().branches.contains_key(&(
        "acme-gl/cforge/app".to_owned(),
        "cforge/github-1".to_owned()
    )));
}

// --- (c): new commits ---------------------------------------------------------------------------------------

#[test]
fn new_commits_move_the_head_and_the_notice_marker_without_a_second_request() {
    let mut c = C::new();
    let n = c.open(Forge::Github, &alice());
    c.event(Forge::Github, n, &alice());
    c.h.gh.borrow_mut().push_pull(GH, n, B);
    let report = c.event(Forge::Github, n, &alice());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    assert_eq!(
        c.heads.borrow().branches[&(
            "acme-gl/cforge/app".to_owned(),
            "cforge/github-1".to_owned()
        )],
        B
    );
    assert_eq!(c.requests(GL_PATH).len(), 1);
    assert_eq!(c.notices.0.borrow().notices.len(), 1);
    assert_eq!(c.notice_marker().sha, B);
    // And the move is itself idempotent.
    let before = c.writes();
    c.event(Forge::Github, n, &alice());
    assert_eq!(c.writes(), before);
}

// --- (d): closing ---------------------------------------------------------------------------------------------

#[test]
fn closing_at_the_origin_closes_the_siblings_and_the_notice() {
    let mut c = C::new();
    let n = c.open(Forge::Github, &alice());
    c.event(Forge::Github, n, &alice());
    c.h.gh.borrow_mut().set_pull_state(GH, n, ReqState::Closed);
    let report = c.event(Forge::Github, n, &alice());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    assert_eq!(c.requests(GL_PATH), vec![(1, ReqState::Closed)]);
    assert!(
        c.heads.borrow().branches.is_empty(),
        "the relay head is removed"
    );
    // Closed, never deleted (R50).
    assert_eq!(c.notices.0.borrow().notices.len(), 1);
    assert!(c.notices.0.borrow().closed.contains(&1));
    let before = c.writes();
    c.event(Forge::Github, n, &alice());
    assert_eq!(c.writes(), before);
}

#[test]
fn take_in_closes_every_sibling_and_the_notice_as_merged() {
    let mut c = C::new();
    let n = c.open(Forge::Gitlab, &bob());
    c.event(Forge::Gitlab, n, &bob());
    let hint = c.hint(Forge::Gitlab, n, &bob());
    let report = c.h.eng().take_in_contribution(&hint, B).unwrap();
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    assert_eq!(c.requests(GH_PATH), vec![(1, ReqState::Closed)]);
    assert_eq!(c.pull_state(Forge::Gitlab, n), ReqState::Closed);
    assert!(c.heads.borrow().branches.is_empty());
    assert!(c.notices.0.borrow().closed.contains(&1));
    // A repeat of the take-in and of the origin's event writes nothing.
    let before = c.writes();
    c.h.eng().take_in_contribution(&hint, B).unwrap();
    c.event(Forge::Gitlab, n, &bob());
    assert_eq!(c.writes(), before);
}

#[test]
fn a_merge_seen_at_the_origin_is_a_take_in() {
    let mut c = C::new();
    let n = c.open(Forge::Github, &alice());
    c.event(Forge::Github, n, &alice());
    // The mirrors carried the reviewed commit; GitHub marks its request merged by itself.
    c.h.gh.borrow_mut().set_pull_state(GH, n, ReqState::Merged);
    let report = c.event(Forge::Github, n, &alice());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    assert_eq!(c.requests(GL_PATH), vec![(1, ReqState::Closed)]);
    assert_eq!(
        c.pull_state(Forge::Github, n),
        ReqState::Merged,
        "left as merged"
    );
    assert!(c.notices.0.borrow().closed.contains(&1));
    let before = c.writes();
    c.event(Forge::Github, n, &alice());
    assert_eq!(c.writes(), before);
}

#[test]
fn a_forgejo_origin_request_is_closed_by_take_in_too() {
    let mut c = C::new();
    let n = c.open(Forge::Forgejo, &alice());
    c.event(Forge::Forgejo, n, &alice());
    let hint = c.hint(Forge::Forgejo, n, &alice());
    c.h.eng().take_in_contribution(&hint, B).unwrap();
    assert_eq!(c.requests(GH_PATH), vec![(1, ReqState::Closed)]);
    assert_eq!(c.requests(GL_PATH), vec![(1, ReqState::Closed)]);
    assert_eq!(c.pull_state(Forge::Forgejo, n), ReqState::Closed);
    assert!(c.notices.0.borrow().closed.contains(&1));
}

// --- comments ---------------------------------------------------------------------------------------------------

#[test]
fn comments_on_the_origin_reach_the_sibling_once_with_credit() {
    let mut c = C::new();
    let n = c.open(Forge::Github, &alice());
    c.event(Forge::Github, n, &alice());
    c.h.gh
        .borrow_mut()
        .comment(GH, n, &bob(), "Looks good to me.");
    let report = c.event(Forge::Github, n, &bob());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    let mr = c.h.gl.borrow_mut().issue(GL, 1);
    assert_eq!(mr.comments.len(), 1);
    assert!(mr.comments[0]
        .body
        .starts_with("**bob** wrote this on GitHub:"));
    assert!(mr.comments[0].body.contains("Looks good to me."));
    let before = c.writes();
    c.event(Forge::Github, n, &bob());
    assert_eq!(c.writes(), before, "no duplicate comment");
}

#[test]
fn a_comment_on_a_sibling_reaches_the_origin_through_its_head() {
    let mut c = C::new();
    let n = c.open(Forge::Github, &alice());
    c.event(Forge::Github, n, &alice());
    c.h.gl
        .borrow_mut()
        .comment(GL, 1, &bob(), "Please add a test.");
    // The hint names the GitLab sibling; its relay head leads back to the GitHub origin.
    let report = c.event(Forge::Gitlab, 1, &bob());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    let origin = c.h.gh.borrow_mut().issue(GH, n);
    assert_eq!(origin.comments.len(), 1);
    assert!(origin.comments[0].body.contains("Please add a test."));
    // The relayed copy is not relayed back.
    let before = c.writes();
    c.event(Forge::Github, n, &alice());
    c.event(Forge::Gitlab, 1, &bob());
    assert_eq!(c.writes(), before);
}

// --- guards ---------------------------------------------------------------------------------------------------------

#[test]
fn a_held_contribution_keeps_its_notice_but_has_no_sibling() {
    let mut c = C::new();
    c.h.hold_all_screen();
    let n = c.open(Forge::Github, &alice());
    let report = c.event(Forge::Github, n, &alice());
    assert!(matches!(report.outcome, Outcome::Held(_)), "{report:?}");
    assert!(c.requests(GL_PATH).is_empty());
    assert!(c.heads.borrow().branches.is_empty());
    assert_eq!(
        c.notices.0.borrow().notices.len(),
        1,
        "the reviewer needs the notice"
    );
}

#[test]
fn the_author_cap_holds_the_second_request_of_a_day() {
    let mut c = C::new();
    c.h.cfg.limits.author_fanout_per_day = 1;
    let n1 = c.open(Forge::Github, &alice());
    assert_eq!(
        c.event(Forge::Github, n1, &alice()).outcome,
        Outcome::Converged
    );
    let n2 = c.open(Forge::Github, &alice());
    let report = c.event(Forge::Github, n2, &alice());
    assert!(matches!(report.outcome, Outcome::Held(_)), "{report:?}");
    assert_eq!(c.requests(GL_PATH).len(), 1);
    assert_eq!(c.notices.0.borrow().notices.len(), 2);
}

#[test]
fn a_contributed_project_is_refused_before_any_request() {
    let mut c = C::new();
    c.h.contributed(&["github.com/acme-gh/app"]);
    let n = c.open(Forge::Github, &alice());
    let report = c.event(Forge::Github, n, &alice());
    assert!(
        matches!(report.outcome, Outcome::Dropped(ref r) if r.contains("R11")),
        "{report:?}"
    );
    assert_eq!(c.h.gh.borrow().requests(), 0);
    assert!(c.notices.0.borrow().notices.is_empty());
}

#[test]
fn the_relays_own_requests_are_never_origins() {
    let mut c = C::new();
    let n = c.open(Forge::Github, &alice());
    c.event(Forge::Github, n, &alice());
    let before = c.writes();
    // The webhook of the sibling the relay itself opened.
    let relay = Actor {
        id: "300".into(),
        login: "relay-bot".into(),
    };
    let report = c.event(Forge::Gitlab, 1, &relay);
    assert!(matches!(report.outcome, Outcome::Dropped(_)), "{report:?}");
    assert_eq!(c.writes(), before);
}

#[test]
fn bitbucket_requests_are_not_relayed() {
    let mut c = C::new();
    let report = c.event(Forge::Bitbucket, 1, &alice());
    assert!(matches!(report.outcome, Outcome::Dropped(_)), "{report:?}");
    assert_eq!(c.h.bb.borrow().requests(), 0);
}

#[test]
fn a_deleted_request_is_dropped_and_its_siblings_stay() {
    let mut c = C::new();
    let n = c.open(Forge::Github, &alice());
    c.event(Forge::Github, n, &alice());
    c.h.gh.borrow_mut().delete_issue(GH, n);
    let report = c.event(Forge::Github, n, &alice());
    assert!(matches!(report.outcome, Outcome::Dropped(_)), "{report:?}");
    assert_eq!(c.requests(GL_PATH), vec![(1, ReqState::Open)]);
}

#[test]
fn a_hold_on_the_copy_namespace_leaves_the_sibling_pending_without_a_request() {
    let mut c = C::new();
    c.h.store
        .hold_put(super::model::Hold {
            forge: Forge::Gitlab,
            namespace: "acme-gl".into(),
            reason: "flagged".into(),
            since: 1,
        })
        .unwrap();
    let n = c.open(Forge::Github, &alice());
    let report = c.event(Forge::Github, n, &alice());
    assert!(matches!(report.outcome, Outcome::Pending(_)), "{report:?}");
    assert!(c.requests(GL_PATH).is_empty());
    assert_eq!(
        c.notices.0.borrow().notices.len(),
        1,
        "the notice is Forgejo's own"
    );
}

#[test]
fn the_credit_line_follows_the_project_choice() {
    let mut c = C::new();
    c.h.cfg.repos[0].credit = super::model::Credit::Plain;
    let n = c.open(Forge::Github, &alice());
    c.event(Forge::Github, n, &alice());
    let mr = c.h.gl.borrow_mut().issue(GL, 1);
    assert!(mr
        .snap
        .body
        .starts_with("Relayed from GitHub: https://github.example/gh1/pull/1"));
}
