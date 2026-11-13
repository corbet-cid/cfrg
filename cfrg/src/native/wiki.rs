//! The wiki of a copy (operating model R40; Forge-router §5.9). The wiki is written on
//! Forgejo only. `cfrg native --operation wiki` pushes it to the copies whose forge holds a
//! wiki for free and can receive it without a web-UI write; every other copy links to it
//! from its description ([`super::links`]). The decisions here are pure.
//!
//! | Copy | Leg | Why |
//! |---|---|---|
//! | GitLab | push | free for public and private projects; a project deploy key with `can_push` may push the wiki (probe RW1, 2026-11-13) |
//! | GitHub | link | a GitHub wiki repository exists only after a first page is made in the web UI, and R9 rules out UI writes; the copy keeps `has_wiki` off |
//! | Bitbucket | link | Bitbucket Cloud removed Wikis on 2026-08-20 |
//!
//! The push is a fast-forward or nothing. A copy wiki that carries a commit Forgejo does
//! not have is held and reported, never overwritten: someone wrote there, and their text
//! is not ours to drop.
use super::Provider;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Leg {
    Push,
    Link,
}

pub fn leg(provider: Provider) -> Leg {
    match provider {
        Provider::Gitlab => Leg::Push,
        Provider::Github | Provider::Bitbucket => Leg::Link,
    }
}

/// The copy wiki's branch as `git ls-remote` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Copy {
    /// No branch at all (a wiki nobody has written yet).
    Empty,
    /// The branch the source uses, at this commit.
    At(String),
    /// Only other branches (the copy's wiki lives on a branch the source does not use).
    OtherBranch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    InSync,
    /// First push into an empty copy wiki.
    Seed,
    FastForward,
    Held(&'static str),
}

impl Action {
    pub fn writes(self) -> bool {
        matches!(self, Action::Seed | Action::FastForward)
    }
}

/// `source`: the commit of the Forgejo wiki's branch. `ancestor`: whether the copy's commit
/// is an ancestor of `source` (`None` when it was not determined, which never pushes).
pub fn decide(source: &str, copy: &Copy, ancestor: Option<bool>) -> Action {
    match copy {
        Copy::Empty => Action::Seed,
        Copy::OtherBranch => Action::Held("copy-wiki-on-another-branch"),
        Copy::At(head) if head == source => Action::InSync,
        Copy::At(_) => match ancestor {
            Some(true) => Action::FastForward,
            Some(false) => Action::Held("copy-wiki-diverged"),
            None => Action::Held("copy-wiki-ancestry-unknown"),
        },
    }
}

/// The copy's state from `git ls-remote` output (`<sha>\t<ref>` lines) for `branch`.
pub fn read_copy(ls_remote: &str, branch: &str) -> Copy {
    let wanted = format!("refs/heads/{branch}");
    let mut other = false;
    for line in ls_remote.lines() {
        let Some((sha, name)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = name.trim();
        if name == wanted {
            return Copy::At(sha.trim().to_string());
        }
        if name.starts_with("refs/heads/") {
            other = true;
        }
    }
    if other {
        Copy::OtherBranch
    } else {
        Copy::Empty
    }
}

/// The source's branch and commit from `git ls-remote --symref <url> HEAD`.
pub fn read_source_head(ls_remote: &str) -> Option<(String, String)> {
    let mut branch = None;
    let mut sha = None;
    for line in ls_remote.lines() {
        if let Some(rest) = line.strip_prefix("ref: refs/heads/") {
            branch = rest.split_whitespace().next().map(String::from);
        } else if let Some((s, name)) = line.split_once(char::is_whitespace) {
            if name.trim() == "HEAD" {
                sha = Some(s.trim().to_string());
            }
        }
    }
    let sha = sha.filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))?;
    let branch = branch.filter(|b| !b.is_empty() && !b.starts_with('-') && !b.contains(".."))?;
    Some((branch, sha))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn only_gitlab_receives_the_wiki_the_others_link() {
        assert_eq!(leg(Provider::Gitlab), Leg::Push);
        assert_eq!(leg(Provider::Github), Leg::Link);
        assert_eq!(leg(Provider::Bitbucket), Leg::Link);
    }

    #[test]
    fn an_empty_copy_is_seeded_and_an_equal_one_is_left_alone() {
        assert_eq!(decide(A, &Copy::Empty, None), Action::Seed);
        assert_eq!(decide(A, &Copy::At(A.into()), None), Action::InSync);
        assert!(!Action::InSync.writes());
    }

    #[test]
    fn a_copy_behind_the_source_is_fast_forwarded() {
        assert_eq!(
            decide(A, &Copy::At(B.into()), Some(true)),
            Action::FastForward
        );
        assert!(Action::FastForward.writes() && Action::Seed.writes());
    }

    #[test]
    fn a_diverged_or_unknown_copy_is_held_never_overwritten() {
        for (ancestor, reason) in [
            (Some(false), "copy-wiki-diverged"),
            (None, "copy-wiki-ancestry-unknown"),
        ] {
            let action = decide(A, &Copy::At(B.into()), ancestor);
            assert_eq!(action, Action::Held(reason));
            assert!(!action.writes());
        }
        assert_eq!(
            decide(A, &Copy::OtherBranch, None),
            Action::Held("copy-wiki-on-another-branch")
        );
    }

    #[test]
    fn the_copy_is_read_from_ls_remote() {
        let text = format!("{A}\tHEAD\n{A}\trefs/heads/main\n");
        assert_eq!(read_copy(&text, "main"), Copy::At(A.into()));
        assert_eq!(read_copy("", "main"), Copy::Empty);
        let other = format!("{B}\trefs/heads/master\n");
        assert_eq!(read_copy(&other, "main"), Copy::OtherBranch);
        assert_eq!(
            read_copy(&format!("{B}\trefs/tags/v1\n"), "main"),
            Copy::Empty
        );
    }

    #[test]
    fn the_source_head_comes_from_the_symref_and_is_validated() {
        let text = format!("ref: refs/heads/main\tHEAD\n{A}\tHEAD\n");
        assert_eq!(read_source_head(&text), Some(("main".into(), A.into())));
        assert_eq!(read_source_head(&format!("{A}\tHEAD\n")), None);
        assert_eq!(
            read_source_head("ref: refs/heads/main\tHEAD\nnot-a-sha\tHEAD\n"),
            None
        );
        assert_eq!(
            read_source_head(&format!("ref: refs/heads/--upload-pack\tHEAD\n{A}\tHEAD\n")),
            None
        );
    }
}
