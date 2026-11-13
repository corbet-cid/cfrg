//! `cfrg take-in`: take one reviewed contribution into the primary (R36, R52).
//!
//! Run by the owner's agent with the owner's git credential after the owner's decision. It fetches the reviewed commit
//! from the contribution's origin into a scratch clone of the primary, refuses if the origin's head is no longer the
//! reviewed commit, then fast-forwards the primary branch to it, or makes a merge commit when the branch has moved on.
//! Never a rebase, never a squash, never a force push: the commits keep their identity, so the copies' pull requests close
//! as merged once the mirroring carries the branch to them. A merge that conflicts is refused and nothing is pushed.
use crate::gate::failure;
use cfrg::Result;
use clap::Args;
use std::{path::Path, process::Command};

const AUTHOR_NAME: &str = "Julian Y. Richard Corbet";
const AUTHOR_EMAIL: &str = "48553615+julian-corbet@users.noreply.github.com";

#[derive(Args)]
pub struct Options {
    /// Git URL (or path) of the primary repository; pushed with the owner's own git credential.
    #[arg(long)]
    primary: String,
    /// Branch of the primary that receives the contribution.
    #[arg(long, default_value = "main")]
    branch: String,
    /// Git URL (or path) of the contribution's origin.
    #[arg(long)]
    origin: String,
    /// Ref of the contribution at its origin (a branch, or a pull/merge request head ref).
    #[arg(long = "ref")]
    origin_ref: String,
    /// The commit the review covered (40 lowercase hex); the take-in refuses if the origin's ref is elsewhere.
    #[arg(long)]
    reviewed: String,
    /// Do everything except the push.
    #[arg(long)]
    dry_run: bool,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
        .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL)
        .output()?;
    if !out.status.success() {
        return Err(failure(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// What a take-in did.
#[derive(Debug, PartialEq, Eq)]
pub enum Landed {
    FastForward(String),
    Merge(String),
}

pub fn run(options: Options) -> Result<()> {
    let scratch = tempfile::tempdir()?;
    let landed = take_in(&options, scratch.path())?;
    match landed {
        Landed::FastForward(head) => println!("fast-forward to {head}"),
        Landed::Merge(head) => println!("merge commit {head}"),
    }
    Ok(())
}

pub fn take_in(options: &Options, scratch: &Path) -> Result<Landed> {
    if !is_sha(&options.reviewed) {
        return Err(failure("--reviewed must be a lowercase 40-hex commit"));
    }
    for (name, value) in [
        ("--primary", &options.primary),
        ("--origin", &options.origin),
    ] {
        if value.starts_with('-') {
            return Err(failure(format!("{name} must not start with a dash")));
        }
    }
    let branch = options.branch.as_str();
    if branch.starts_with('-') || branch.contains("..") {
        return Err(failure("invalid --branch"));
    }
    git(scratch, &["init", "--quiet", "."])?;
    git(scratch, &["remote", "add", "primary", &options.primary])?;
    git(scratch, &["remote", "add", "origin", &options.origin])?;
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
            &format!("+{}:refs/take-in/head", options.origin_ref),
        ],
    )?;
    let head = git(scratch, &["rev-parse", "refs/take-in/head"])?;
    if head != options.reviewed {
        return Err(failure(format!(
            "refused: the contribution moved since the review (reviewed {}, origin now {head})",
            options.reviewed
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
    if options.dry_run {
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
    fn fixture(root: &Path, moved: bool) -> (Options, String) {
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
        let options = Options {
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
}
