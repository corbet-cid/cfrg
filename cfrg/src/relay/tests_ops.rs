//! Deletions, caps, outages, refusals and the sweep, against fake forges (R50, RF1, RF9, RD11).

use super::forge::Refusal;
use super::model::{Actor, HintAction};
use super::tests::{alice, bob, maint, FJ, GH, GL, H};
use super::Outcome;
use crate::model::Forge;

fn delete_on_github(h: &mut H, n: u64, actor: Option<&Actor>) -> super::Report {
    h.gh.borrow_mut().delete_issue(GH, n);
    let hint = h.hint(Forge::Github, n, HintAction::Deleted, actor);
    h.eng().apply(&hint).unwrap()
}

fn issues(h: &H) -> (usize, usize) {
    let live = |w: &super::fake::Handle, id: &str| {
        w.borrow().repos[id]
            .issues
            .values()
            .filter(|i| !i.deleted)
            .count()
    };
    (live(&h.fj, FJ), live(&h.gl, GL))
}

#[test]
fn a_deletion_by_the_author_reaches_every_copy_and_a_late_event_cannot_resurrect_it() {
    let mut h = H::new();
    let n = h.github_issue();
    let report = delete_on_github(&mut h, n, Some(&alice()));
    assert_eq!(report.outcome, Outcome::Deleted, "{report:?}");
    assert_eq!(issues(&h), (0, 0));
    h.clear_logs();
    let late = h.hint(Forge::Github, n, HintAction::Changed, Some(&alice()));
    h.eng().apply(&late).unwrap();
    assert_eq!(issues(&h), (0, 0));
    assert!(h.mutations().is_empty(), "{:?}", h.mutations());
}

#[test]
fn a_maintainer_may_delete_but_a_stranger_or_an_unattributed_deletion_is_held() {
    let mut h = H::new();
    let n = h.github_issue();
    let report = delete_on_github(&mut h, n, Some(&bob()));
    assert!(
        matches!(report.outcome, Outcome::Held(ref r) if r.contains("neither the author nor a maintainer")),
        "{report:?}"
    );
    assert_eq!(issues(&h), (1, 1), "nothing else was deleted");

    let mut h = H::new();
    let n = h.github_issue();
    let report = delete_on_github(&mut h, n, None);
    assert!(
        matches!(report.outcome, Outcome::Held(ref r) if r.contains("did not say by whom")),
        "{report:?}"
    );
    assert_eq!(issues(&h), (1, 1));

    let mut h = H::new();
    let n = h.github_issue();
    assert_eq!(
        delete_on_github(&mut h, n, Some(&maint())).outcome,
        Outcome::Deleted
    );
    assert_eq!(issues(&h), (0, 0));
}

#[test]
fn a_deletion_event_without_a_definitive_gone_deletes_nothing() {
    let mut h = H::new();
    let n = h.github_issue();
    // The issue still exists: the event is only a hint and the pass converges instead.
    let hint = h.hint(Forge::Github, n, HintAction::Deleted, Some(&alice()));
    assert_eq!(h.eng().apply(&hint).unwrap().outcome, Outcome::Converged);
    assert_eq!(issues(&h), (1, 1));
    // The forge is down: the deletion cannot be confirmed.
    h.gh.borrow_mut().delete_issue(GH, n);
    h.gh.borrow_mut().faults.down = true;
    assert!(matches!(
        h.eng().apply(&hint).unwrap().outcome,
        Outcome::Deferred(_)
    ));
    assert_eq!(issues(&h), (1, 1));
}

#[test]
fn at_most_three_deletions_per_repository_per_hour_are_cascaded() {
    let mut h = H::new();
    let numbers: Vec<u64> = (0..4).map(|_| h.github_issue()).collect();
    for n in &numbers[..3] {
        assert_eq!(
            delete_on_github(&mut h, *n, Some(&alice())).outcome,
            Outcome::Deleted
        );
    }
    let report = delete_on_github(&mut h, numbers[3], Some(&alice()));
    assert!(
        matches!(report.outcome, Outcome::Held(ref r) if r.contains("cascade cap")),
        "{report:?}"
    );
    assert_eq!(issues(&h), (1, 1), "the fourth copy set survives");
}

#[test]
fn at_most_ten_deletions_per_hour_fleet_wide_is_the_default_and_the_limit_is_enforced() {
    assert_eq!(super::model::Limits::default().cascade_deletes_per_hour, 10);
    assert_eq!(
        super::model::Limits::default().cascade_deletes_per_repo_per_hour,
        3
    );
    let mut cfg = super::tests::config();
    cfg.limits.cascade_deletes_per_hour = 2;
    cfg.limits.cascade_deletes_per_repo_per_hour = 100;
    let mut h = H::with(cfg);
    let numbers: Vec<u64> = (0..3).map(|_| h.github_issue()).collect();
    assert_eq!(
        delete_on_github(&mut h, numbers[0], Some(&alice())).outcome,
        Outcome::Deleted
    );
    assert_eq!(
        delete_on_github(&mut h, numbers[1], Some(&alice())).outcome,
        Outcome::Deleted
    );
    let report = delete_on_github(&mut h, numbers[2], Some(&alice()));
    assert!(
        matches!(report.outcome, Outcome::Held(ref r) if r.contains("limit 2")),
        "{report:?}"
    );
    // An hour later the window has moved on; the owner releases the held one.
    h.clock.advance_secs(3_601);
    let lid = h.lid(Forge::Github, numbers[2]);
    assert!(h.eng().release_object(&lid).unwrap());
}

#[test]
fn a_comment_deletion_by_a_stranger_is_held_and_by_its_author_is_carried_over() {
    let mut h = H::new();
    let n = h.github_issue();
    let c = h.gh.borrow_mut().comment(GH, n, &bob(), "my comment");
    let hint = h.hint(Forge::Github, n, HintAction::Changed, Some(&bob()));
    assert_eq!(h.eng().apply(&hint).unwrap().outcome, Outcome::Converged);
    assert_eq!(
        h.gl.borrow_mut().issue(GL, 1).comments.len(),
        1,
        "the relayed comment"
    );
    h.gh.borrow_mut().remove_comment(GH, n, c);
    // Alice is neither the comment's author nor a maintainer: held, nothing is deleted elsewhere.
    let hint = h.hint(
        Forge::Github,
        n,
        HintAction::CommentDeleted { comment_id: c },
        Some(&alice()),
    );
    let report = h.eng().apply(&hint).unwrap();
    assert!(matches!(report.outcome, Outcome::Held(_)), "{report:?}");
    assert_eq!(h.gl.borrow_mut().issue(GL, 1).comments.len(), 1);
    // A fresh object, the same deletion by the comment's author, is carried over.
    let mut h = H::new();
    let n = h.github_issue();
    let c = h.gh.borrow_mut().comment(GH, n, &bob(), "my comment");
    let hint = h.hint(Forge::Github, n, HintAction::Changed, Some(&bob()));
    h.eng().apply(&hint).unwrap();
    h.gh.borrow_mut().remove_comment(GH, n, c);
    let hint = h.hint(
        Forge::Github,
        n,
        HintAction::CommentDeleted { comment_id: c },
        Some(&bob()),
    );
    assert_eq!(h.eng().apply(&hint).unwrap().outcome, Outcome::Deleted);
    assert_eq!(h.gl.borrow_mut().issue(GL, 1).comments.len(), 0);
}

#[test]
fn a_refusal_holds_the_namespace_and_nothing_more_is_sent_there() {
    let mut h = H::new();
    h.gl.borrow_mut().faults.refuse = Some(Refusal::RateLimited);
    let n = h.gh.borrow_mut().open_issue(GH, &alice(), "t", "b", &[]);
    let report = h.push(Forge::Github, n, &alice());
    assert!(matches!(report.outcome, Outcome::Pending(_)), "{report:?}");
    assert_eq!(h.store.holds().unwrap().len(), 1);
    h.gl.borrow_mut().clear_log();
    h.gl.borrow_mut().faults.refuse = None;
    h.gh.borrow_mut()
        .edit(GH, n, |s| s.title = "changed".into());
    h.push(Forge::Github, n, &alice());
    assert_eq!(
        h.gl.borrow().requests(),
        0,
        "a hold is only lifted by the owner"
    );
}

#[test]
fn an_outage_leaves_an_intent_and_the_sweep_finishes_it_once() {
    let mut h = H::new();
    h.gl.borrow_mut().faults.down = true;
    let n = h.gh.borrow_mut().open_issue(GH, &alice(), "t", "b", &[]);
    let report = h.push(Forge::Github, n, &alice());
    assert!(matches!(report.outcome, Outcome::Pending(_)), "{report:?}");
    assert_eq!(h.store.intents_open().unwrap().len(), 1);
    h.gl.borrow_mut().faults.down = false;
    // Too young: the sweep leaves it alone.
    assert_eq!(h.eng().sweep(None).unwrap().objects, 0);
    h.clock.advance_secs(3_601);
    let swept = h.eng().sweep(None).unwrap();
    assert_eq!(
        (swept.objects, swept.reports[0].outcome.clone()),
        (1, Outcome::Converged),
        "{swept:?}"
    );
    assert_eq!(h.gl.borrow().repos[GL].issues.len(), 1);
    assert!(h.store.intents_open().unwrap().is_empty());
}

#[test]
fn a_lost_answer_is_recovered_by_the_marker_never_by_posting_twice() {
    let mut h = H::new();
    h.gl.borrow_mut().faults.lose_answers = 1;
    let n = h.gh.borrow_mut().open_issue(GH, &alice(), "t", "b", &[]);
    let report = h.push(Forge::Github, n, &alice());
    assert!(matches!(report.outcome, Outcome::Pending(_)), "{report:?}");
    h.clock.advance_secs(3_601);
    h.eng().sweep(None).unwrap();
    assert_eq!(h.gl.borrow().repos[GL].issues.len(), 1, "one copy, not two");
    assert!(h.store.intents_open().unwrap().is_empty());
}

#[test]
fn the_sweep_with_nothing_pending_reads_and_writes_nothing() {
    let mut h = H::new();
    h.github_issue();
    h.clear_logs();
    h.clock.advance_secs(86_400);
    let swept = h.eng().sweep(None).unwrap();
    assert_eq!((swept.objects, swept.left, swept.reports.len()), (0, 0, 0));
    assert_eq!(
        h.fj.borrow().requests() + h.gh.borrow().requests() + h.gl.borrow().requests(),
        0
    );
}

#[test]
fn the_kill_switch_stops_everything_including_reads() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = super::tests::config();
    cfg.kill_switch = Some(dir.path().join("stop"));
    let mut h = H::with(cfg);
    let n = h.gh.borrow_mut().open_issue(GH, &alice(), "t", "b", &[]);
    std::fs::create_dir(dir.path().join("stop")).unwrap();
    let report = h.push(Forge::Github, n, &alice());
    assert!(matches!(report.outcome, Outcome::Deferred(_)), "{report:?}");
    assert_eq!(h.gh.borrow().requests(), 0);
}

#[test]
fn a_contributed_copy_refuses_the_whole_set_before_any_read_or_write() {
    let mut h = H::new();
    h.contributed(&["gitlab.com/acme-gl/app"]);
    // The event comes from the Forgejo primary, but the GitLab copy is a contributed project: nothing is touched.
    let n = h.fj.borrow_mut().open_issue(FJ, &alice(), "t", "b", &[]);
    let hint = h.hint(Forge::Forgejo, n, HintAction::Changed, Some(&alice()));
    let report = h.eng().apply(&hint).unwrap();
    assert!(
        matches!(report.outcome, Outcome::Dropped(ref r) if r.contains("contributed project")),
        "{report:?}"
    );
    assert_eq!(
        h.fj.borrow().requests() + h.gh.borrow().requests() + h.gl.borrow().requests(),
        0
    );
    assert!(h.mutations().is_empty());
}
