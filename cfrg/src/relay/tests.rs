//! Tests of the relay core against fake forges. Every test runs in zero real time (a fake clock).

use super::fake::{FakeForge, Handle};
use super::forge::{Forges, Screen, ScreenVerdict};
use super::marker::{footer_markers, Marker};
use super::model::{
    Actor, Credit, Hint, HintAction, IssueState, Kind, Person, RepoRef, RepoSet, RouterConfig,
};
use super::pacing::testing::FakeClock;
use super::store::{MemStore, Store};
use super::{Engine, Outcome, Report};
use crate::{contributed::Contributed, model::Forge, native::Placement};
use std::{cell::Cell, collections::BTreeMap, rc::Rc};

pub(super) const FJ: &str = "fj1";
pub(super) const GH: &str = "gh1";
pub(super) const GL: &str = "gl1";
pub(super) const BB: &str = "{bb-1}";

pub(super) fn alice() -> Actor {
    Actor {
        id: "u-alice".into(),
        login: "alice".into(),
    }
}

pub(super) fn bob() -> Actor {
    Actor {
        id: "u-bob".into(),
        login: "bob".into(),
    }
}

pub(super) fn maint() -> Actor {
    Actor {
        id: "u-maint".into(),
        login: "maint".into(),
    }
}

pub(super) struct PassAll;

impl Screen for PassAll {
    fn screen(&self, _forge: Forge, _object: &super::model::ObjectState) -> ScreenVerdict {
        ScreenVerdict::Pass
    }
}

/// The real `cfrg::contributed` index of a placement that declares these `host/path` locations as contributed projects.
fn contributed_of(paths: &[&str]) -> Contributed {
    let list: Vec<String> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| format!(r#"{{"name":"p{i}","upstream":"https://{p}"}}"#))
        .collect();
    let document = format!(
        r#"{{"native":{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"repositories":[],"contributed":[{}]}}}}"#,
        list.join(",")
    );
    Contributed::from_placement(&Placement::from_document(document.as_bytes()).unwrap(), &[])
        .unwrap()
}

pub(super) fn config() -> RouterConfig {
    let member = |host: &str, id: &str, path: &str| RepoRef {
        host: host.into(),
        id: id.into(),
        path: path.into(),
    };
    RouterConfig {
        relay: BTreeMap::from([
            (Forge::Forgejo, vec!["100".into()]),
            (Forge::Github, vec!["200".into()]),
            (Forge::Gitlab, vec!["300".into()]),
            (Forge::Bitbucket, vec!["400".into()]),
        ]),
        repos: vec![RepoSet {
            name: "acme/app".into(),
            credit: Credit::Highlight,
            members: BTreeMap::from([
                (Forge::Forgejo, member("forge.example", FJ, "acme/app")),
                (Forge::Github, member("github.com", GH, "acme-gh/app")),
                (Forge::Gitlab, member("gitlab.com", GL, "acme-gl/app")),
                (Forge::Bitbucket, member("bitbucket.org", BB, "acme-bb/app")),
            ]),
        }],
        people: vec![Person {
            key: "ada".into(),
            name: "Ada Lovelace".into(),
            handles: BTreeMap::from([
                (Forge::Github, "adal".into()),
                (Forge::Gitlab, "ada-gl".into()),
                (Forge::Forgejo, "ada".into()),
            ]),
        }],
        ..RouterConfig::default()
    }
}

pub(super) struct H {
    pub cfg: RouterConfig,
    pub store: Box<dyn Store>,
    pub forges: Forges,
    pub clock: FakeClock,
    pub fj: Handle,
    pub gh: Handle,
    pub gl: Handle,
    pub bb: Handle,
    screen: Box<dyn Screen>,
    contributed: Contributed,
}

impl H {
    pub fn new() -> Self {
        Self::with(config())
    }

    pub fn with(cfg: RouterConfig) -> Self {
        Self::on(cfg, Box::<MemStore>::default())
    }

    pub fn on(cfg: RouterConfig, store: Box<dyn Store>) -> Self {
        let stamp = Rc::new(Cell::new(0));
        let (fj_f, fj) = FakeForge::new(Forge::Forgejo, "100", Rc::clone(&stamp));
        let (gh_f, gh) = FakeForge::new(Forge::Github, "200", Rc::clone(&stamp));
        let (gl_f, gl) = FakeForge::new(Forge::Gitlab, "300", Rc::clone(&stamp));
        let (bb_f, bb) = FakeForge::new(Forge::Bitbucket, "400", Rc::clone(&stamp));
        let mut forges = Forges::new();
        for f in [fj_f, gh_f, gl_f, bb_f] {
            forges.insert(Box::new(f));
        }
        for (world, id) in [(&fj, FJ), (&gh, GH), (&gl, GL), (&bb, BB)] {
            world.borrow_mut().repo(id).maintainers.insert(maint().id);
        }
        Self {
            cfg,
            store,
            forges,
            clock: FakeClock::at(1_000_000),
            fj,
            gh,
            gl,
            bb,
            screen: Box::new(PassAll),
            contributed: contributed_of(&[]),
        }
    }

    pub fn hold_all_screen(&mut self) {
        self.screen = Box::new(super::forge::HoldAll);
    }

    pub fn contributed(&mut self, paths: &[&str]) {
        self.contributed = contributed_of(paths);
    }

    pub fn eng(&mut self) -> Engine<'_> {
        Engine {
            cfg: &self.cfg,
            store: &mut *self.store,
            forges: &mut self.forges,
            clock: &self.clock,
            screen: &*self.screen,
            contributed: &self.contributed,
            holder: "test",
        }
    }

    pub fn repo_id(forge: Forge) -> &'static str {
        match forge {
            Forge::Forgejo => FJ,
            Forge::Github => GH,
            Forge::Gitlab => GL,
            Forge::Bitbucket => BB,
        }
    }

    pub fn hint(
        &mut self,
        forge: Forge,
        number: u64,
        action: HintAction,
        actor: Option<&Actor>,
    ) -> Hint {
        Hint {
            forge,
            repo: Self::repo_id(forge).into(),
            kind: Kind::Issue,
            number,
            action,
            actor: actor.cloned(),
        }
    }

    /// One event arrives and the job applies it.
    pub fn push(&mut self, forge: Forge, number: u64, actor: &Actor) -> Report {
        let hint = self.hint(forge, number, HintAction::Changed, Some(actor));
        self.push_hint(hint)
    }

    pub fn push_hint(&mut self, hint: Hint) -> Report {
        self.eng().apply(&hint).unwrap()
    }

    pub fn lid(&self, forge: Forge, number: u64) -> String {
        self.store
            .sibling_at(forge, Self::repo_id(forge), number)
            .unwrap()
            .expect("a tracked sibling")
            .lid
    }

    pub fn clear_logs(&self) {
        for w in [&self.fj, &self.gh, &self.gl, &self.bb] {
            w.borrow_mut().clear_log();
        }
    }

    pub fn mutations(&self) -> Vec<String> {
        [&self.fj, &self.gh, &self.gl, &self.bb]
            .iter()
            .flat_map(|w| w.borrow().mutations())
            .collect()
    }

    /// Opens an issue on GitHub as `alice` and relays it everywhere.
    pub fn github_issue(&mut self) -> u64 {
        let n = self.gh.borrow_mut().open_issue(
            GH,
            &alice(),
            "Crash on start",
            "It crashes, see #7.",
            &["bug"],
        );
        let report = self.push(Forge::Github, n, &alice());
        assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
        n
    }
}

// --- R37, R38, RD2: creation and fan-out --------------------------------------------------------------

#[test]
fn an_issue_opened_on_github_appears_on_forgejo_and_gitlab_hub_first() {
    let mut h = H::new();
    let n = h.github_issue();
    let hub = h.fj.borrow_mut().issue(FJ, 1);
    let gitlab = h.gl.borrow_mut().issue(GL, 1);
    for (issue, forge) in [(&hub, Forge::Forgejo), (&gitlab, Forge::Gitlab)] {
        assert_eq!(issue.snap.title, "Crash on start");
        assert_eq!(issue.snap.labels, ["bug".to_owned()].into());
        assert!(
            issue.snap.body.starts_with(
                "**alice** wrote this on GitHub: https://github.example/gh1/issues/1\n"
            ),
            "{}",
            issue.snap.body
        );
        // The origin's text is copied; the reference to #7 links to the origin because #7 is not tracked.
        assert!(
            issue
                .snap
                .body
                .contains("It crashes, see [#7](https://github.example/gh1/issues/7)."),
            "{}",
            issue.snap.body
        );
        let markers = footer_markers(&issue.snap.body);
        assert!(markers
            .iter()
            .any(|m| matches!(m, Marker::Object { forge: f, .. } if *f == forge)));
    }
    assert_eq!(hub.author.id, "100");
    assert_eq!(gitlab.author.id, "300");
    // The hub lists the origin and the GitLab copy; the origin got exactly one cforge comment.
    assert!(hub.snap.body.contains("Also on: [GitHub #1]"));
    assert!(hub.snap.body.contains("[GitLab #1]"));
    let origin = h.gh.borrow_mut().issue(GH, n);
    assert_eq!(origin.snap.title, "Crash on start");
    assert_eq!(
        origin.snap.body, "It crashes, see #7.",
        "the origin's text is never rewritten"
    );
    assert_eq!(origin.comments.len(), 1);
    assert_eq!(origin.comments[0].author.id, "200");
    assert!(
        origin.comments[0].body.contains("[Forgejo #1]")
            && origin.comments[0].body.contains("[GitLab #1]")
    );
    // RD2: the hub is written before any spoke.
    let first_hub = h.fj.borrow().mutation_stamps()[0].0;
    let first_gitlab = h.gl.borrow().mutation_stamps()[0].0;
    assert!(first_hub < first_gitlab);
    // Bitbucket has no issues (model section 3.5): nothing was created there.
    assert_eq!(h.bb.borrow().requests(), 0);
}

#[test]
fn a_converged_object_costs_no_write() {
    let mut h = H::new();
    let n = h.github_issue();
    h.clear_logs();
    let again = h.push(Forge::Github, n, &alice());
    assert_eq!(again.outcome, Outcome::Converged);
    assert_eq!(again.writes, 0);
    assert!(h.mutations().is_empty(), "RD4(b): {:?}", h.mutations());
    assert_eq!(again.pending, 0);
}

#[test]
fn the_first_issue_takes_the_minimum_of_writes() {
    let mut h = H::new();
    h.github_issue();
    // hub create, gitlab create, hub body update (now lists gitlab), cforge sibling-list comment on the origin.
    assert_eq!(h.mutations().len(), 4, "{:?}", h.mutations());
}

#[test]
fn writes_to_one_namespace_are_spaced_by_the_gap_and_creations_by_the_longer_one() {
    let mut h = H::new();
    h.github_issue();
    h.github_issue();
    let mut by_ns: std::collections::BTreeMap<(Forge, String), Vec<(u64, String)>> =
        Default::default();
    for e in h.store.ledger_since(0).unwrap() {
        if e.kind == super::model::LedgerKind::Write {
            by_ns
                .entry((e.forge, e.namespace))
                .or_default()
                .push((e.ts, e.lid.unwrap()));
        }
    }
    assert!(!by_ns.is_empty());
    for ((forge, ns), mut writes) in by_ns {
        writes.sort();
        for pair in writes.windows(2) {
            assert!(pair[1].0 - pair[0].0 >= 2, "{forge:?} {ns}: {writes:?}");
        }
        // The first write of each object in a namespace is its creation (or the origin's first comment).
        let mut firsts: Vec<u64> = Vec::new();
        for (ts, lid) in &writes {
            if !writes.iter().any(|(t, l)| l == lid && t < ts) {
                firsts.push(*ts);
            }
        }
        if forge != Forge::Github {
            assert_eq!(firsts.len(), 2, "{forge:?} {ns}");
            assert!(firsts[1] - firsts[0] >= 12, "{forge:?} {ns}: {firsts:?}");
        }
    }
}

#[test]
fn an_issue_opened_on_forgejo_is_its_own_hub() {
    let mut h = H::new();
    let n =
        h.fj.borrow_mut()
            .open_issue(FJ, &alice(), "From the hub", "text", &[]);
    let report = h.push(Forge::Forgejo, n, &alice());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    assert_eq!(h.gh.borrow_mut().issue(GH, 1).snap.title, "From the hub");
    assert_eq!(h.gl.borrow_mut().issue(GL, 1).snap.title, "From the hub");
    let hub = h.fj.borrow_mut().issue(FJ, n);
    assert_eq!(
        hub.snap.body, "text",
        "the human-authored hub issue is never rewritten"
    );
    assert_eq!(
        hub.comments.len(),
        1,
        "the sibling list is a cforge comment on it"
    );
}

// --- RD4, RD1, policy: guards -----------------------------------------------------------------------------

#[test]
fn a_delivery_from_the_relay_identity_is_dropped_before_any_request() {
    let mut h = H::new();
    let relay = Actor {
        id: "200".into(),
        login: "corbet-forge".into(),
    };
    let hint = h.hint(Forge::Github, 1, HintAction::Changed, Some(&relay));
    let report = h.eng().apply(&hint).unwrap();
    assert!(
        matches!(report.outcome, Outcome::Dropped(ref r) if r.contains("relay")),
        "{report:?}"
    );
    assert_eq!(h.gh.borrow().requests(), 0);
}

#[test]
fn a_duplicate_delivery_writes_nothing() {
    let mut h = H::new();
    let n = h.github_issue();
    h.clear_logs();
    let hint = h.hint(Forge::Github, n, HintAction::Changed, Some(&alice()));
    let report = h.eng().apply(&hint).unwrap();
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    assert_eq!(report.writes, 0);
    assert!(h.mutations().is_empty(), "{:?}", h.mutations());
}

#[test]
fn a_busy_lease_defers_the_event_without_any_request() {
    let mut h = H::new();
    let n = h.gh.borrow_mut().open_issue(GH, &alice(), "t", "b", &[]);
    let hint = h.hint(Forge::Github, n, HintAction::Changed, Some(&alice()));
    let lid = super::model::lid_object(&hint.object());
    assert!(h
        .store
        .lease(&lid, "another-job", h.clock.0.get(), 600)
        .unwrap());
    let report = h.eng().apply(&hint).unwrap();
    assert!(matches!(report.outcome, Outcome::Deferred(_)), "{report:?}");
    assert_eq!(h.gh.borrow().requests() + h.fj.borrow().requests(), 0);
    // An expired lease does not block.
    h.clock.advance_secs(601);
    assert_eq!(h.eng().apply(&hint).unwrap().outcome, Outcome::Converged);
}

#[test]
fn a_repository_that_is_not_listed_is_never_read_or_written() {
    let mut h = H::new();
    let mut hint = h.hint(Forge::Github, 1, HintAction::Changed, Some(&alice()));
    hint.repo = "some-other-repo".into();
    let report = h.eng().apply(&hint).unwrap();
    assert!(
        matches!(report.outcome, Outcome::Dropped(ref r) if r.contains("not relayed")),
        "{report:?}"
    );
    assert_eq!(h.gh.borrow().requests(), 0);
}

#[test]
fn a_contributed_project_is_refused_before_any_request() {
    let mut h = H::new();
    h.contributed(&["github.com/acme-gh/app"]);
    let n = h.gh.borrow_mut().open_issue(GH, &alice(), "t", "b", &[]);
    let hint = h.hint(Forge::Github, n, HintAction::Changed, Some(&alice()));
    let report = h.eng().apply(&hint).unwrap();
    match report.outcome {
        Outcome::Dropped(reason) => assert!(
            reason.starts_with("relay refused:") && reason.contains("R11"),
            "{reason}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        h.gh.borrow().requests() + h.fj.borrow().requests() + h.gl.borrow().requests(),
        0
    );
}

#[test]
fn contribution_flows_are_not_part_of_this_lane() {
    let mut h = H::new();
    let mut hint = h.hint(Forge::Github, 1, HintAction::Changed, Some(&alice()));
    hint.kind = Kind::Contribution;
    let report = h.eng().apply(&hint).unwrap();
    assert!(matches!(report.outcome, Outcome::Unsupported(_)));
}

#[test]
fn a_bitbucket_hint_is_dropped_because_bitbucket_has_no_issues() {
    let mut h = H::new();
    h.bb.borrow_mut().open_issue(BB, &alice(), "t", "b", &[]);
    let report = h.push(Forge::Bitbucket, 1, &alice());
    assert!(
        matches!(report.outcome, Outcome::Dropped(ref r) if r.contains("no issues")),
        "{report:?}"
    );
}

#[test]
fn confidential_issues_are_never_relayed() {
    let mut h = H::new();
    let n =
        h.gh.borrow_mut()
            .open_issue(GH, &alice(), "secret", "b", &[]);
    h.gh.borrow_mut()
        .repo(GH)
        .issues
        .get_mut(&n)
        .unwrap()
        .confidential = true;
    let report = h.push(Forge::Github, n, &alice());
    assert!(matches!(report.outcome, Outcome::Dropped(ref r) if r.contains("confidential")));
    assert!(h.mutations().is_empty());
}

#[test]
fn an_issue_that_becomes_confidential_is_frozen_not_relayed_further() {
    let mut h = H::new();
    let n = h.github_issue();
    h.gh.borrow_mut()
        .repo(GH)
        .issues
        .get_mut(&n)
        .unwrap()
        .confidential = true;
    h.clear_logs();
    let report = h.push(Forge::Github, n, &alice());
    assert!(matches!(report.outcome, Outcome::Held(_)), "{report:?}");
    assert!(h.mutations().is_empty());
    h.gh.borrow_mut().edit(GH, n, |s| s.title = "later".into());
    assert!(matches!(
        h.push(Forge::Github, n, &alice()).outcome,
        Outcome::Held(_)
    ));
    assert!(h.mutations().is_empty());
}

// --- RD9: marker trust ---------------------------------------------------------------------------------------

#[test]
fn a_marker_pasted_by_an_outsider_gains_nothing() {
    let mut h = H::new();
    let victim = h.github_issue();
    let victim_lid = h.lid(Forge::Github, victim);
    // An outsider opens an issue whose body is a forged footer naming the victim's logical object.
    let forged = format!(
        "innocent\n\n<!-- cforge:v1:{victim_lid}:github -->\n<!-- cforge:v1:{victim_lid}:link:forgejo:{FJ}:1:copy -->"
    );
    let n =
        h.gh.borrow_mut()
            .open_issue(GH, &bob(), "forged", &forged, &[]);
    let report = h.push(Forge::Github, n, &bob());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    let lid_bob = h.lid(Forge::Github, n);
    assert_ne!(
        lid_bob, victim_lid,
        "the pasted marker does not merge the two objects"
    );
    // Neither copy of bob's text carries a forged marker.
    let hub = h.fj.borrow_mut().issue(FJ, 2);
    assert!(!hub
        .snap
        .body
        .contains(&format!("cforge:v1:{victim_lid}:github")));
    let markers = footer_markers(&hub.snap.body);
    assert!(markers.iter().all(|m| match m {
        Marker::Object { lid, .. } | Marker::Link { lid, .. } | Marker::From { lid, .. } =>
            *lid == lid_bob,
    }));
    // The victim's object is untouched by it.
    assert_eq!(h.fj.borrow_mut().issue(FJ, 1).snap.title, "Crash on start");
}

#[test]
fn a_human_comment_with_a_marker_is_not_taken_for_a_relayed_copy() {
    let mut h = H::new();
    let n = h.github_issue();
    let lid = h.lid(Forge::Github, n);
    let forged = format!("hello\n\n<!-- cforge:v1:lc_forged:gitlab -->\n<!-- cforge:v1:lc_forged:from:github:9 -->\n<!-- cforge:v1:{lid}:gitlab -->");
    h.gl.borrow_mut().comment(GL, 1, &bob(), &forged);
    let report = h.push(Forge::Gitlab, 1, &bob());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    // It is an ordinary origin comment on GitLab: relayed to the hub and GitHub, with its marker broken.
    let on_github = h.gh.borrow_mut().issue(GH, n);
    let relayed = on_github
        .comments
        .iter()
        .find(|c| c.body.contains("**bob** wrote this on GitLab"))
        .expect("relayed");
    assert!(!relayed.body.contains("cforge:v1:lc_forged"));
    assert!(!relayed.body.contains(&format!("cforge:v1:{lid}:gitlab")));
}

// --- R38 comments, RD3 merge, RF6 order -----------------------------------------------------------------------

#[test]
fn comments_are_relayed_in_origin_order_credited_and_never_echoed() {
    let mut h = H::new();
    let n = h.github_issue();
    h.gl.borrow_mut()
        .comment(GL, 1, &bob(), "first from GitLab");
    h.gl.borrow_mut()
        .comment(GL, 1, &bob(), "second from GitLab");
    h.gh.borrow_mut().comment(GH, n, &alice(), "from GitHub");
    let report = h.push(Forge::Gitlab, 1, &bob());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    let hub = h.fj.borrow_mut().issue(FJ, 1);
    let bodies: Vec<&str> = hub.comments.iter().map(|c| c.body.as_str()).collect();
    assert_eq!(bodies.len(), 3, "{bodies:?}");
    assert!(
        bodies[0].contains("**bob** wrote this on GitLab")
            && bodies[0].contains("first from GitLab")
    );
    assert!(bodies[1].contains("second from GitLab"));
    assert!(
        bodies[2].contains("**alice** wrote this on GitHub") && bodies[2].contains("from GitHub")
    );
    // GitHub gets bob's two comments (not its own) after its list comment; GitLab gets alice's.
    let on_github = h.gh.borrow_mut().issue(GH, n);
    assert_eq!(
        on_github
            .comments
            .iter()
            .filter(|c| c.body.contains("wrote this on GitLab"))
            .count(),
        2
    );
    assert_eq!(
        on_github
            .comments
            .iter()
            .filter(|c| c.body.contains("wrote this on GitHub"))
            .count(),
        0
    );
    // Nothing is echoed back on any later pass.
    h.clear_logs();
    for forge in [Forge::Forgejo, Forge::Github, Forge::Gitlab] {
        let r = h.push(forge, 1, &alice());
        assert_eq!(r.writes, 0, "{forge:?}: {r:?}");
    }
    assert!(h.mutations().is_empty(), "{:?}", h.mutations());
}

#[test]
fn an_edited_comment_is_updated_on_every_copy() {
    let mut h = H::new();
    let n = h.github_issue();
    let id = h.gh.borrow_mut().comment(GH, n, &alice(), "typo");
    h.push(Forge::Github, n, &alice());
    h.gh.borrow_mut().edit_comment(GH, n, id, "fixed");
    let report = h.push(Forge::Github, n, &alice());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    for issue in [
        h.fj.borrow_mut().issue(FJ, 1),
        h.gl.borrow_mut().issue(GL, 1),
    ] {
        let copy = issue
            .comments
            .iter()
            .find(|c| c.body.contains("wrote this on GitHub"))
            .expect("copy");
        assert!(
            copy.body.contains("fixed") && !copy.body.contains("typo"),
            "{}",
            copy.body
        );
    }
}

#[test]
fn labels_merge_by_additions_and_removals_across_forges() {
    let mut h = H::new();
    let n = h.github_issue();
    h.gh.borrow_mut().edit(GH, n, |s| {
        s.labels.insert("ui".into());
    });
    h.gl.borrow_mut().edit(GL, 1, |s| {
        s.labels.remove("bug");
        s.labels.insert("p1".into());
    });
    // The two changes are processed one after the other; both survive (RF7: label sets merge).
    h.push(Forge::Github, n, &alice());
    let report = h.push(Forge::Gitlab, 1, &maint());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    let want: std::collections::BTreeSet<String> = ["ui".to_owned(), "p1".to_owned()].into();
    assert_eq!(h.gh.borrow_mut().issue(GH, n).snap.labels, want);
    assert_eq!(h.gl.borrow_mut().issue(GL, 1).snap.labels, want);
    assert_eq!(h.fj.borrow_mut().issue(FJ, 1).snap.labels, want);
}

#[test]
fn close_and_reopen_reach_every_forge_and_the_last_processed_wins() {
    let mut h = H::new();
    let n = h.github_issue();
    h.fj.borrow_mut()
        .edit(FJ, 1, |s| s.state = Some(IssueState::Closed));
    h.push(Forge::Forgejo, 1, &maint());
    for issue in [
        h.gh.borrow_mut().issue(GH, n),
        h.gl.borrow_mut().issue(GL, 1),
    ] {
        assert_eq!(issue.snap.state, Some(IssueState::Closed));
    }
    // RF7: closed on GitLab and reopened on GitHub in the same window; the hinted (last processed) wins.
    h.gl.borrow_mut()
        .edit(GL, 1, |s| s.state = Some(IssueState::Open));
    h.gh.borrow_mut()
        .edit(GH, n, |s| s.state = Some(IssueState::Open));
    h.push(Forge::Github, n, &alice());
    for issue in [
        h.fj.borrow_mut().issue(FJ, 1),
        h.gl.borrow_mut().issue(GL, 1),
    ] {
        assert_eq!(issue.snap.state, Some(IssueState::Open));
    }
}

#[test]
fn an_edit_of_the_origin_title_and_body_reaches_the_copies_but_a_copy_edit_does_not_travel() {
    let mut h = H::new();
    let n = h.github_issue();
    h.gh.borrow_mut().edit(GH, n, |s| {
        s.title = "Crash on startup".into();
        s.body = "New text".into();
    });
    h.push(Forge::Github, n, &alice());
    let hub = h.fj.borrow_mut().issue(FJ, 1);
    assert_eq!(hub.snap.title, "Crash on startup");
    assert!(hub.snap.body.contains("New text") && !hub.snap.body.contains("It crashes"));
    // A maintainer edits a copy's title: it is not copied to the origin or the other copy (R50: by their author).
    h.gl.borrow_mut()
        .edit(GL, 1, |s| s.title = "edited on the copy".into());
    h.clear_logs();
    h.push(Forge::Gitlab, 1, &maint());
    assert_eq!(
        h.gh.borrow_mut().issue(GH, n).snap.title,
        "Crash on startup"
    );
    assert_eq!(
        h.fj.borrow_mut().issue(FJ, 1).snap.title,
        "Crash on startup"
    );
    assert!(
        h.gh.borrow().mutations().is_empty(),
        "the origin is never written for text"
    );
}

#[test]
fn assignees_travel_only_where_the_identity_table_knows_the_person() {
    let mut h = H::new();
    let n = h.github_issue();
    h.gh.borrow_mut().edit(GH, n, |s| {
        s.assignees.insert("adal".into());
        s.assignees.insert("stranger".into());
    });
    h.push(Forge::Github, n, &alice());
    assert_eq!(
        h.gl.borrow_mut().issue(GL, 1).snap.assignees,
        ["ada-gl".to_owned()].into()
    );
    assert_eq!(
        h.fj.borrow_mut().issue(FJ, 1).snap.assignees,
        ["ada".to_owned()].into()
    );
}

// --- R51: abuse gate -------------------------------------------------------------------------------------------

#[test]
fn without_a_screen_the_object_stays_on_forgejo_held() {
    let mut h = H::new();
    h.hold_all_screen();
    let n =
        h.gh.borrow_mut()
            .open_issue(GH, &alice(), "maybe spam", "buy now", &[]);
    let report = h.push(Forge::Github, n, &alice());
    assert!(
        matches!(report.outcome, Outcome::Held(ref r) if r.contains("no abuse screen")),
        "{report:?}"
    );
    let hub = h.fj.borrow_mut().issue(FJ, 1);
    assert!(hub.snap.labels.contains("held"), "marked held on Forgejo");
    assert_eq!(h.gl.borrow().requests(), 0, "no fan-out");
    assert!(
        h.gh.borrow_mut().issue(GH, n).comments.is_empty(),
        "no sibling list on the origin either"
    );
}

#[test]
fn the_owner_releases_a_held_object_and_it_fans_out() {
    let mut h = H::new();
    h.hold_all_screen();
    let n =
        h.gh.borrow_mut()
            .open_issue(GH, &alice(), "fine after all", "text", &[]);
    h.push(Forge::Github, n, &alice());
    let lid = h.lid(Forge::Github, n);
    assert!(h.eng().release_object(&lid).unwrap());
    assert_eq!(h.gl.borrow_mut().issue(GL, 1).snap.title, "fine after all");
    let hub = h.fj.borrow_mut().issue(FJ, 1);
    assert!(
        !hub.snap.labels.contains("held"),
        "the held mark is removed"
    );
}

#[test]
fn at_most_five_new_objects_per_author_per_day_are_fanned_out() {
    let mut h = H::new();
    for i in 1..=6u64 {
        let n =
            h.gh.borrow_mut()
                .open_issue(GH, &alice(), &format!("issue {i}"), "text", &[]);
        let report = h.push(Forge::Github, n, &alice());
        if i <= 5 {
            assert_eq!(report.outcome, Outcome::Converged, "{i}: {report:?}");
        } else {
            assert!(
                matches!(report.outcome, Outcome::Held(ref r) if r.contains("R51")),
                "{report:?}"
            );
        }
    }
    assert_eq!(h.gl.borrow().repos[GL].issues.len(), 5);
    assert_eq!(
        h.fj.borrow().repos[FJ].issues.len(),
        6,
        "the hub copy is made at once, always"
    );
    // Another author is not affected; a day later the cap has reset.
    let n =
        h.gh.borrow_mut()
            .open_issue(GH, &bob(), "bob's", "text", &[]);
    assert_eq!(h.push(Forge::Github, n, &bob()).outcome, Outcome::Converged);
    h.clock.advance_secs(86_400 + 1);
    let n =
        h.gh.borrow_mut()
            .open_issue(GH, &alice(), "next day", "text", &[]);
    assert_eq!(
        h.push(Forge::Github, n, &alice()).outcome,
        Outcome::Converged
    );
}

// --- RD10: the identity of a logical object is derived, not drawn --------------------------------------------

#[test]
fn a_repository_rename_changes_nothing_because_repositories_are_addressed_by_id() {
    let mut h = H::new();
    let n = h.github_issue();
    h.cfg.repos[0].members.get_mut(&Forge::Github).unwrap().path = "renamed-org/renamed".into();
    h.gh.borrow_mut()
        .edit(GH, n, |s| s.title = "after the rename".into());
    h.clear_logs();
    let report = h.push(Forge::Github, n, &alice());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    assert_eq!(
        h.fj.borrow_mut().issue(FJ, 1).snap.title,
        "after the rename"
    );
}

#[test]
fn a_gitlab_issue_becomes_a_forgejo_and_github_sibling_and_an_edit_and_a_close_propagate() {
    let mut h = H::new();
    let n =
        h.gl.borrow_mut()
            .open_issue(GL, &alice_gl(), "Hangs on exit", "It hangs.", &["bug"]);
    let report = h.push(Forge::Gitlab, n, &alice_gl());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    let hub = h.fj.borrow_mut().issue(FJ, 1);
    let gh = h.gh.borrow_mut().issue(GH, 1);
    assert_eq!(hub.snap.title, "Hangs on exit");
    assert_eq!(gh.snap.title, "Hangs on exit");
    assert!(gh.snap.labels.contains("bug"));
    assert_eq!(
        hub.author.id, "100",
        "the sibling is written by the relay identity"
    );
    assert_eq!(gh.author.id, "200");

    // An edit of the origin reaches both siblings.
    h.gl.borrow_mut().edit(GL, n, |s| {
        s.title = "Hangs on exit (Linux)".into();
        s.body = "It hangs on Linux.".into();
    });
    h.clear_logs();
    let report = h.push(Forge::Gitlab, n, &alice_gl());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    for issue in [
        h.fj.borrow_mut().issue(FJ, 1),
        h.gh.borrow_mut().issue(GH, 1),
    ] {
        assert_eq!(issue.snap.title, "Hangs on exit (Linux)");
        assert!(issue.snap.body.contains("It hangs on Linux."));
    }

    // A close reaches both siblings.
    h.gl.borrow_mut()
        .edit(GL, n, |s| s.state = Some(IssueState::Closed));
    let report = h.push(Forge::Gitlab, n, &alice_gl());
    assert_eq!(report.outcome, Outcome::Converged, "{report:?}");
    for issue in [
        h.fj.borrow_mut().issue(FJ, 1),
        h.gh.borrow_mut().issue(GH, 1),
    ] {
        assert_eq!(issue.snap.state, Some(IssueState::Closed));
    }
    // A repeated delivery of the same close writes nothing.
    h.clear_logs();
    h.push(Forge::Gitlab, n, &alice_gl());
    assert!(h.mutations().is_empty(), "{:?}", h.mutations());
}

fn alice_gl() -> Actor {
    Actor {
        id: "9001".into(),
        login: "ada-gl".into(),
    }
}
