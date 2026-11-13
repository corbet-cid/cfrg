//! `cfrg take-in`: take one reviewed contribution into the primary (R36, R52).
//!
//! Run by the owner's agent with the owner's git credential after the owner's decision. It fetches the reviewed commit
//! from the contribution's origin into a scratch clone of the primary, refuses if the origin's head is no longer the
//! reviewed commit, then fast-forwards the primary branch to it, or makes a merge commit when the branch has moved on.
//! Never a rebase, never a squash, never a force push: the commits keep their identity, so the copies' pull requests close
//! as merged once the mirroring carries the branch to them. A merge that conflicts is refused and nothing is pushed.
use crate::gate::failure;
use cfrg::{
    relay::{
        model::{IssueState, ObjectState},
        notice::{latest_review_head, ContributionMarker, ReviewComment},
    },
    Result,
};
use clap::Args;
use std::{path::Path, process::Command};

/// `cfrg take-in <owner/repo>#<notice>`: everything else is read from the notice.
#[derive(Args)]
pub struct Options {
    /// The notice: `<owner/repo>#<number>` on the Forgejo primary.
    target: String,
    /// Forgejo API base, for example `https://forge.example.org`.
    #[arg(long)]
    forge: String,
    /// Host that serves the primary's git URL (pushed with the owner's own git credential).
    #[arg(long)]
    primary_host: String,
    /// Login whose review comments count.
    #[arg(long)]
    reviewer: String,
    /// Do everything except the push.
    #[arg(long)]
    dry_run: bool,
}

/// What a take-in does, fully resolved: the notice gave `origin`, `origin_ref` and `reviewed`. Tests build it directly.
#[derive(Debug)]
pub struct Plan {
    /// Git URL (or path) of the primary repository.
    pub primary: String,
    /// Branch of the primary that receives the contribution.
    pub branch: String,
    /// Git URL (or path) of the contribution's origin.
    pub origin: String,
    /// Ref of the contribution at its origin.
    pub origin_ref: String,
    /// The commit the review covered (40 lowercase hex); the take-in refuses if the origin's ref is elsewhere.
    pub reviewed: String,
    pub dry_run: bool,
}

/// A neutral commit identity, used only when git has none configured (the operator's own configured identity wins).
fn fallback_identity(dir: &Path, key: &str, value: &str) -> Option<(String, String)> {
    let configured = Command::new("git")
        .current_dir(dir)
        .args(["config", "--get", key])
        .output()
        .map(|o| o.status.success() && !o.stdout.iter().all(u8::is_ascii_whitespace))
        .unwrap_or(false);
    let (author, committer) = if key == "user.name" {
        ("GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME")
    } else {
        ("GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL")
    };
    let unset = |var: &str| std::env::var_os(var).is_none();
    (!configured && unset(author) && unset(committer)).then(|| (key.to_owned(), value.to_owned()))
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    for (key, value) in [
        ("user.name", "cfrg take-in"),
        ("user.email", "take-in@example.invalid"),
    ] {
        if let Some((key, value)) = fallback_identity(dir, key, value) {
            let (author, committer) = if key == "user.name" {
                ("GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME")
            } else {
                ("GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL")
            };
            command.env(author, &value).env(committer, &value);
        }
    }
    let out = command.output()?;
    if !out.status.success() {
        return Err(failure(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// What a take-in did.
#[derive(Debug, PartialEq, Eq)]
pub enum Landed {
    FastForward(String),
    Merge(String),
}

/// Reads what the notice says: where the commits are and which head the latest review covered. Refuses a notice that is
/// not an open `contribution`, has no valid marker, has no review by `reviewer`, or whose newest head (the marker's `sha`)
/// is not the reviewed one: a head that moved after the review needs a new review first.
pub fn plan_from_notice(
    notice: &ObjectState,
    reviewer: &str,
    primary: &str,
    branch: &str,
    dry_run: bool,
) -> Result<Plan> {
    if notice.snap.state != Some(IssueState::Open) || !notice.snap.labels.contains("contribution") {
        return Err(failure("refused: not an open contribution notice"));
    }
    let marker = ContributionMarker::parse(&notice.snap.body)
        .ok_or_else(|| failure("refused: the notice has no valid cfrg-contribution marker"))?;
    let comments: Vec<ReviewComment> = notice
        .comments
        .iter()
        .map(|c| ReviewComment {
            author: c.author.login.clone(),
            id: c.id,
            body: c.body.clone(),
        })
        .collect();
    let reviewed = latest_review_head(&comments, reviewer)
        .ok_or_else(|| failure(format!("refused: no review by {reviewer} on the notice")))?;
    if reviewed != marker.sha {
        return Err(failure(format!(
            "refused: the contribution moved since the review (reviewed {reviewed}, notice now {}); wait for the new review",
            marker.sha
        )));
    }
    Ok(Plan {
        primary: primary.to_owned(),
        branch: branch.to_owned(),
        origin: marker.origin,
        origin_ref: marker.reference,
        reviewed,
        dry_run,
    })
}

/// The Forgejo token that reads the notice: `CFRG_FORGEJO_TOKEN` (an environment variable only, never an argument).
fn forgejo_token() -> Result<String> {
    match std::env::var("CFRG_FORGEJO_TOKEN") {
        Ok(token) if !token.trim().is_empty() => Ok(token.trim().to_owned()),
        _ => Err(failure("no Forgejo token: set CFRG_FORGEJO_TOKEN")),
    }
}

/// The primary's default branch, asked of the primary itself.
fn default_branch(primary: &str) -> Result<String> {
    let out = Command::new("git")
        .args(["ls-remote", "--symref", primary, "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.strip_prefix("ref: refs/heads/"))
        .and_then(|l| l.split_whitespace().next())
        .map(str::to_owned)
        .ok_or_else(|| failure("cannot read the primary's default branch"))
}

pub fn run(options: Options) -> Result<()> {
    let (repo, number) = options
        .target
        .split_once('#')
        .and_then(|(r, n)| Some((r, n.parse::<i64>().ok()?)))
        .ok_or_else(|| failure("expected <owner/repo>#<notice number>"))?;
    let rt = cfrg_relay::live::runtime()?;
    let token = forgejo_token()?;
    let driver = cfgj::relay::ForgejoRelay::new(&options.forge, &cfgj::relay::Token::new(token))
        .map_err(|e| failure(e.to_string()))?;
    let forge_repo = cfgj::relay::Repo::parse(repo).map_err(|e| failure(e.to_string()))?;
    let notice = rt
        .block_on(driver.issue_read(&forge_repo, number))
        .map_err(|e| failure(e.to_string()))?
        .ok_or_else(|| failure("refused: the notice does not exist"))?;
    let primary = format!("{}/{repo}.git", options.primary_host.trim_end_matches('/'));
    let branch = default_branch(&primary)?;
    let plan = plan_from_notice(
        &notice,
        &options.reviewer,
        &primary,
        &branch,
        options.dry_run,
    )?;
    let scratch = tempfile::tempdir()?;
    match take_in(&plan, scratch.path())? {
        Landed::FastForward(head) => println!("fast-forward to {head}"),
        Landed::Merge(head) => println!("merge commit {head}"),
    }
    Ok(())
}

pub fn take_in(plan: &Plan, scratch: &Path) -> Result<Landed> {
    if !cfrg::relay::notice::is_sha(&plan.reviewed) {
        return Err(failure(
            "the reviewed commit must be a lowercase 40-hex commit",
        ));
    }
    for (name, value) in [("primary", &plan.primary), ("origin", &plan.origin)] {
        if value.starts_with('-') {
            return Err(failure(format!("{name} must not start with a dash")));
        }
    }
    let branch = plan.branch.as_str();
    if branch.starts_with('-') || branch.contains("..") {
        return Err(failure("invalid branch"));
    }
    git(scratch, &["init", "--quiet", "."])?;
    git(scratch, &["remote", "add", "primary", &plan.primary])?;
    git(scratch, &["remote", "add", "origin", &plan.origin])?;
    git(
        scratch,
        &[
            "fetch",
            "--quiet",
            "primary",
            &format!("+refs/heads/{branch}:refs/remotes/primary/{branch}"),
        ],
    )?;
    git(
        scratch,
        &[
            "fetch",
            "--quiet",
            "origin",
            &format!("+{}:refs/take-in/head", plan.origin_ref),
        ],
    )?;
    let head = git(scratch, &["rev-parse", "refs/take-in/head"])?;
    if head != plan.reviewed {
        return Err(failure(format!(
            "refused: the contribution moved since the review (reviewed {}, origin now {head})",
            plan.reviewed
        )));
    }
    let base = format!("refs/remotes/primary/{branch}");
    git(scratch, &["checkout", "--quiet", "-B", branch, &base])?;
    let landed = if git(scratch, &["merge-base", "--is-ancestor", "HEAD", &head]).is_ok() {
        git(scratch, &["merge", "--quiet", "--ff-only", &head])?;
        Landed::FastForward(head.clone())
    } else {
        let message = format!("Take in contribution {}", &head[..12]);
        if let Err(e) = git(
            scratch,
            &["merge", "--quiet", "--no-ff", "-m", &message, &head],
        ) {
            return Err(failure(format!(
                "refused: the merge does not apply cleanly ({e})"
            )));
        }
        Landed::Merge(git(scratch, &["rev-parse", "HEAD"])?)
    };
    if plan.dry_run {
        return Ok(landed);
    }
    // A plain push: if the primary moved meanwhile, git refuses and nothing is overwritten.
    git(
        scratch,
        &[
            "push",
            "--quiet",
            "primary",
            &format!("{branch}:refs/heads/{branch}"),
        ],
    )?;
    Ok(landed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) -> String {
        git(dir, args).unwrap()
    }

    /// A bare primary with one commit, plus a contributor clone with one extra commit on a branch.
    fn fixture_in(root: &Path) -> (Plan, String) {
        fixture(root, false)
    }

    fn fixture(root: &Path, moved: bool) -> (Plan, String) {
        let primary = root.join("primary.git");
        std::fs::create_dir(&primary).unwrap();
        sh(
            &primary,
            &["init", "--quiet", "--bare", "--initial-branch=main"],
        );
        let work = root.join("work");
        std::fs::create_dir(&work).unwrap();
        sh(&work, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(work.join("a"), "a").unwrap();
        sh(&work, &["add", "a"]);
        sh(&work, &["commit", "--quiet", "-m", "base"]);
        sh(
            &work,
            &["push", "--quiet", primary.to_str().unwrap(), "main"],
        );
        sh(&work, &["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(work.join(if moved { "b" } else { "c" }), "x").unwrap();
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "--quiet", "-m", "feature"]);
        let sha = sh(&work, &["rev-parse", "HEAD"]);
        if moved {
            sh(&work, &["checkout", "--quiet", "main"]);
            std::fs::write(work.join("m"), "m").unwrap();
            sh(&work, &["add", "."]);
            sh(&work, &["commit", "--quiet", "-m", "primary moved"]);
            sh(
                &work,
                &["push", "--quiet", primary.to_str().unwrap(), "main"],
            );
        }
        let options = Plan {
            primary: primary.to_str().unwrap().into(),
            branch: "main".into(),
            origin: work.to_str().unwrap().into(),
            origin_ref: "refs/heads/feature".into(),
            reviewed: sha.clone(),
            dry_run: false,
        };
        (options, sha)
    }

    #[test]
    fn fast_forwards_when_the_primary_has_not_moved() {
        let root = tempfile::tempdir().unwrap();
        let (options, sha) = fixture(root.path(), false);
        let scratch = tempfile::tempdir().unwrap();
        assert_eq!(
            take_in(&options, scratch.path()).unwrap(),
            Landed::FastForward(sha.clone())
        );
        let primary = root.path().join("primary.git");
        assert_eq!(sh(&primary, &["rev-parse", "main"]), sha);
    }

    #[test]
    fn merges_with_both_parents_when_the_primary_moved() {
        let root = tempfile::tempdir().unwrap();
        let (options, sha) = fixture(root.path(), true);
        let scratch = tempfile::tempdir().unwrap();
        let Landed::Merge(head) = take_in(&options, scratch.path()).unwrap() else {
            panic!("expected a merge");
        };
        let primary = root.path().join("primary.git");
        assert_eq!(sh(&primary, &["rev-parse", "main"]), head);
        let parents = sh(&primary, &["rev-list", "--parents", "-n1", "main"]);
        assert!(parents.contains(&sha), "the reviewed commit is a parent");
    }

    #[test]
    fn refuses_when_the_head_moved_since_the_review() {
        let root = tempfile::tempdir().unwrap();
        let (mut options, _) = fixture(root.path(), false);
        options.reviewed = "0".repeat(40);
        let scratch = tempfile::tempdir().unwrap();
        let error = take_in(&options, scratch.path()).unwrap_err().to_string();
        assert!(error.contains("moved since the review"), "{error}");
        let primary = root.path().join("primary.git");
        assert!(!sh(&primary, &["log", "--format=%s", "main"]).contains("feature"));
    }

    #[test]
    fn dry_run_pushes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let (mut options, _) = fixture(root.path(), false);
        options.dry_run = true;
        let scratch = tempfile::tempdir().unwrap();
        take_in(&options, scratch.path()).unwrap();
        let primary = root.path().join("primary.git");
        assert!(!sh(&primary, &["log", "--format=%s", "main"]).contains("feature"));
    }

    mod notice {
        use super::super::*;
        use super::fixture_in;
        use cfrg::relay::model::{Actor, CommentState, Snap};

        const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        fn marker(sha: &str) -> String {
            ContributionMarker {
                origin: "https://github.com/o/r.git".into(),
                reference: "refs/pull/3/head".into(),
                sha: sha.into(),
            }
            .attach("text")
        }
        fn review(id: u64, who: &str, sha: &str) -> CommentState {
            CommentState {
                id,
                url: String::new(),
                body: format!("<!-- cfrg-review head={sha} -->\nreport"),
                author: Actor {
                    id: "1".into(),
                    login: who.into(),
                },
                created_at: id,
            }
        }
        fn notice(body: String, comments: Vec<CommentState>) -> ObjectState {
            ObjectState {
                number: 5,
                url: String::new(),
                author: Actor {
                    id: "9".into(),
                    login: "relay".into(),
                },
                snap: Snap {
                    title: "t".into(),
                    body,
                    state: Some(IssueState::Open),
                    labels: ["contribution".to_owned()].into(),
                    assignees: Default::default(),
                },
                confidential: false,
                comments,
                updated_at: 0,
            }
        }
        fn plan(n: &ObjectState) -> Result<Plan> {
            plan_from_notice(n, "me", "https://git/o/r.git", "main", false)
        }

        #[test]
        fn takes_origin_ref_and_the_latest_reviewed_head_from_the_notice() {
            let n = notice(marker(B), vec![review(1, "me", A), review(2, "me", B)]);
            let p = plan(&n).unwrap();
            assert_eq!(
                (
                    p.origin.as_str(),
                    p.origin_ref.as_str(),
                    p.reviewed.as_str()
                ),
                ("https://github.com/o/r.git", "refs/pull/3/head", B)
            );
        }

        #[test]
        fn refuses_when_the_head_moved_after_the_latest_review() {
            let n = notice(marker(B), vec![review(1, "me", A)]);
            assert!(plan(&n)
                .unwrap_err()
                .to_string()
                .contains("moved since the review"));
        }

        #[test]
        fn ignores_other_authors_and_refuses_without_a_review() {
            let n = notice(marker(A), vec![review(1, "outsider", A)]);
            assert!(plan(&n).unwrap_err().to_string().contains("no review"));
        }

        #[test]
        fn refuses_closed_unlabelled_or_unmarked_notices() {
            let mut n = notice(marker(A), vec![review(1, "me", A)]);
            n.snap.state = Some(IssueState::Closed);
            assert!(plan(&n).is_err());
            let mut n = notice(marker(A), vec![review(1, "me", A)]);
            n.snap.labels.clear();
            assert!(plan(&n).is_err());
            let n = notice("no marker here".into(), vec![review(1, "me", A)]);
            assert!(plan(&n).unwrap_err().to_string().contains("marker"));
        }

        #[test]
        fn a_notice_plan_lands_the_reviewed_commit() {
            let root = tempfile::tempdir().unwrap();
            let (mut base, sha) = fixture_in(root.path());
            let n = notice(
                ContributionMarker {
                    origin: "https://x/y.git".into(),
                    reference: "refs/heads/feature".into(),
                    sha: sha.clone(),
                }
                .attach("t"),
                vec![review(1, "me", &sha)],
            );
            let mut p = plan(&n).unwrap();
            // the fake forge is the local fixture: swap in its paths, keep what the notice decided
            p.primary = std::mem::take(&mut base.primary);
            p.origin = std::mem::take(&mut base.origin);
            let scratch = tempfile::tempdir().unwrap();
            assert_eq!(
                take_in(&p, scratch.path()).unwrap(),
                Landed::FastForward(sha)
            );
        }
    }
}
