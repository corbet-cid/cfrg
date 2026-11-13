use super::fake_heads::{FakeGit, FakeHost, Shared, World};
use super::forge::ForgeError;
use super::heads::*;
use crate::model::Forge;
use std::{cell::RefCell, rc::Rc};

fn setup() -> (Heads, Shared) {
    let w: Shared = Rc::new(RefCell::new(World::default()));
    let mut h = Heads::new(Box::new(FakeGit(w.clone())));
    h.insert(Box::new(FakeHost {
        forge: Forge::Github,
        w: w.clone(),
        forge_world: None,
    }));
    h.insert(Box::new(FakeHost {
        forge: Forge::Gitlab,
        w: w.clone(),
        forge_world: None,
    }));
    (h, w)
}

fn origin(sha: &str, state: ReqState) -> Origin {
    Origin {
        forge: Forge::Gitlab,
        repo: "grp/proj".into(),
        number: 7,
        head_sha: sha.into(),
        fetch_url: "origin-url".into(),
        reference: "refs/pull/1/head".into(),
        bodies: Default::default(),
        title: "t".into(),
        body: "b".into(),
        state,
    }
}

fn gh() -> Copy {
    Copy {
        forge: Forge::Github,
        repo: "org/proj".into(),
        base: "main".into(),
    }
}

fn count(w: &Shared, prefix: &str) -> usize {
    w.borrow()
        .log
        .iter()
        .filter(|l| l.starts_with(prefix))
        .count()
}

#[test]
fn names_follow_the_origin_and_stay_in_the_relay_namespace() {
    assert_eq!(branch_name(Forge::Github, 12), "cforge/github-12");
    assert!(is_relay_branch(&branch_name(Forge::Gitlab, 3)));
    assert!(!is_relay_branch("main"));
}

#[test]
fn github_copy_gets_a_relay_branch_and_a_request_once() {
    let (mut h, w) = setup();
    let r = h.sync(&origin("aaa", ReqState::Open), &[gh()]);
    assert!(matches!(r[0].result, Ok(HeadAction::Created(_))));
    assert_eq!(
        w.borrow().branches[&("org/proj".into(), "cforge/gitlab-7".into())],
        "aaa"
    );
    let r = h.sync(&origin("aaa", ReqState::Open), &[gh()]);
    assert!(matches!(r[0].result, Ok(HeadAction::Unchanged(_))));
    assert_eq!((count(&w, "push"), count(&w, "open")), (1, 1));
}

#[test]
fn new_commits_move_the_head_without_a_second_request() {
    let (mut h, w) = setup();
    h.sync(&origin("aaa", ReqState::Open), &[gh()]);
    let r = h.sync(&origin("bbb", ReqState::Open), &[gh()]);
    assert!(matches!(r[0].result, Ok(HeadAction::Updated { .. })));
    assert_eq!(
        w.borrow().branches[&("org/proj".into(), "cforge/gitlab-7".into())],
        "bbb"
    );
    assert_eq!(count(&w, "open"), 1);
}

#[test]
fn gitlab_copy_uses_a_fork_in_the_cforge_subgroup_created_once() {
    let (mut h, w) = setup();
    let o = Origin {
        forge: Forge::Github,
        repo: "org/proj".into(),
        number: 4,
        ..origin("aaa", ReqState::Open)
    };
    let c = Copy {
        forge: Forge::Gitlab,
        repo: "grp/proj".into(),
        base: "main".into(),
    };
    assert!(matches!(
        h.sync(&o, std::slice::from_ref(&c))[0].result,
        Ok(HeadAction::Created(_))
    ));
    assert!(w
        .borrow()
        .branches
        .contains_key(&("grp/cforge/proj".into(), "cforge/github-4".into())));
    // A second pull request of the same group finds the subgroup and the fork: no second creation.
    let o2 = Origin { number: 5, ..o };
    h.sync(&o2, &[c]);
    assert_eq!((count(&w, "group_create"), count(&w, "fork")), (1, 1));
    assert_eq!(count(&w, "open"), 2);
}

#[test]
fn the_origin_forge_is_not_a_target_of_its_own_request() {
    let (mut h, _w) = setup();
    let c = Copy {
        forge: Forge::Gitlab,
        repo: "grp/proj".into(),
        base: "main".into(),
    };
    assert!(h.sync(&origin("aaa", ReqState::Open), &[c]).is_empty());
}

#[test]
fn a_head_that_is_already_gone_is_pushed_again() {
    let (mut h, w) = setup();
    h.sync(&origin("aaa", ReqState::Open), &[gh()]);
    // GitHub closes the request when its branch is deleted.
    w.borrow_mut().branches.clear();
    w.borrow_mut()
        .requests
        .values_mut()
        .for_each(|r| r.2 = ReqState::Closed);
    let r = h.sync(&origin("aaa", ReqState::Open), &[gh()]);
    assert!(matches!(r[0].result, Ok(HeadAction::Recreated(_))));
    assert_eq!(count(&w, "open"), 2);
    assert_eq!(w.borrow().branches.len(), 1);
}

#[test]
fn a_request_a_person_closed_stays_closed_while_the_branch_stands() {
    let (mut h, w) = setup();
    h.sync(&origin("aaa", ReqState::Open), &[gh()]);
    w.borrow_mut()
        .requests
        .values_mut()
        .for_each(|r| r.2 = ReqState::Closed);
    let r = h.sync(&origin("bbb", ReqState::Open), &[gh()]);
    assert!(matches!(r[0].result, Ok(HeadAction::LeftClosed(_))));
    assert_eq!((count(&w, "push"), count(&w, "open")), (1, 1));
}

#[test]
fn take_in_closes_every_sibling_and_removes_the_heads() {
    let (mut h, w) = setup();
    let cs = [gh()];
    h.sync(&origin("aaa", ReqState::Open), &cs);
    // The origin's own request is open on the origin forge.
    w.borrow_mut().requests.insert(
        ("grp/proj".into(), 7),
        ("grp/proj".into(), "x".into(), ReqState::Open),
    );
    let r = h.take_in(&origin("aaa", ReqState::Open), &cs, "aaa");
    assert_eq!(r.len(), 2);
    assert!(r
        .iter()
        .all(|c| matches!(c.result, Ok(HeadAction::ClosedLeftover))));
    assert!(w.borrow().branches.is_empty());
    // Closing again finds nothing to do and nothing to fail.
    let r = h.take_in(&origin("aaa", ReqState::Open), &cs, "aaa");
    assert!(r.iter().all(|c| c.result.is_ok()));
    assert_eq!(count(&w, "close"), 2);
}

#[test]
fn take_in_leaves_a_request_the_forge_already_marked_merged() {
    let (mut h, w) = setup();
    h.sync(&origin("aaa", ReqState::Open), &[gh()]);
    w.borrow_mut()
        .requests
        .values_mut()
        .for_each(|r| r.2 = ReqState::Merged);
    let r = h.take_in(&origin("aaa", ReqState::Open), &[gh()], "aaa");
    assert!(matches!(r[0].result, Ok(HeadAction::AlreadyMerged)));
    assert_eq!(count(&w, "close"), 0);
}

#[test]
fn take_in_with_a_head_already_gone_is_fine() {
    let (mut h, _w) = setup();
    let r = h.take_in(&origin("aaa", ReqState::Open), &[gh()], "aaa");
    assert!(matches!(r[0].result, Ok(HeadAction::Gone)));
}

#[test]
fn origin_closed_without_merge_retires_the_head() {
    let (mut h, w) = setup();
    h.sync(&origin("aaa", ReqState::Open), &[gh()]);
    let r = h.sync(&origin("aaa", ReqState::Closed), &[gh()]);
    assert!(matches!(r[0].result, Ok(HeadAction::Retired)));
    assert!(w.borrow().branches.is_empty());
    assert_eq!(count(&w, "close"), 1);
}

#[test]
fn one_failing_copy_does_not_stop_the_other() {
    let (mut h, w) = setup();
    w.borrow_mut().fail_next = Some(("open".into(), ForgeError::Unavailable("down".into())));
    let o = Origin {
        forge: Forge::Forgejo,
        repo: "o/p".into(),
        ..origin("aaa", ReqState::Open)
    };
    let cs = [
        gh(),
        Copy {
            forge: Forge::Gitlab,
            repo: "grp/proj".into(),
            base: "main".into(),
        },
    ];
    let r = h.sync(&o, &cs);
    assert!(r[0].result.is_err());
    assert!(matches!(r[1].result, Ok(HeadAction::Created(_))));
    // The retry converges: the branch already stands, only the request is missing.
    let r = h.sync(&o, &cs);
    assert!(matches!(r[0].result, Ok(HeadAction::Created(_))));
}

#[test]
fn a_forge_without_a_host_is_an_outage() {
    let w: Shared = Rc::new(RefCell::new(World::default()));
    let mut h = Heads::new(Box::new(FakeGit(w)));
    let r = h.sync(&origin("a", ReqState::Open), &[gh()]);
    assert!(matches!(r[0].result, Err(ForgeError::Unavailable(_))));
}

#[test]
fn git_cli_pushes_exactly_the_named_commit() {
    let t = tempfile::tempdir().unwrap();
    let run = |d: &std::path::Path, a: &[&str]| {
        let o = std::process::Command::new("git")
            .arg("-C")
            .arg(d)
            .args(a)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .unwrap();
        assert!(o.status.success(), "{a:?}");
        String::from_utf8(o.stdout).unwrap().trim().to_string()
    };
    let src = t.path().join("src");
    let dst = t.path().join("dst.git");
    std::fs::create_dir(&src).unwrap();
    run(&src, &["init", "-q", "-b", "main"]);
    run(&src, &["commit", "-q", "--allow-empty", "-m", "one"]);
    let sha = run(&src, &["rev-parse", "HEAD"]);
    run(&src, &["commit", "-q", "--allow-empty", "-m", "two"]);
    std::fs::create_dir(&dst).unwrap();
    run(&dst, &["init", "-q", "--bare"]);
    GitCli
        .push_head(
            src.to_str().unwrap(),
            "refs/heads/main",
            &sha,
            dst.to_str().unwrap(),
            "cforge/github-1",
        )
        .unwrap();
    assert_eq!(run(&dst, &["rev-parse", "refs/heads/cforge/github-1"]), sha);
}

#[test]
fn a_skipped_copy_is_neither_written_nor_an_error() {
    let (mut h, w) = setup();
    let c = Copy {
        forge: Forge::Gitlab,
        repo: "personal/proj".into(),
        base: "main".into(),
    };
    let o = Origin {
        forge: Forge::Github,
        repo: "org/proj".into(),
        ..origin("aaa", ReqState::Open)
    };
    assert!(matches!(
        h.sync(&o, std::slice::from_ref(&c))[0].result,
        Ok(HeadAction::Skipped(_))
    ));
    assert!(matches!(
        h.take_in(&o, &[c], "aaa")[0].result,
        Ok(HeadAction::Skipped(_))
    ));
    assert!(w.borrow().log.is_empty());
}
