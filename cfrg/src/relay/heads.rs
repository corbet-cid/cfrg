//! Pull request heads of the relay (Bridge design §3.3, build task B8).
//!
//! An outside pull request on one copy (GitHub or GitLab) is shown on the other copy as a real request
//! whose head the relay owns:
//!
//! - GitHub copy: the head is the branch `cforge/<origin>-<n>` inside the copy. The GitHub write gate
//!   never prunes that namespace, so the branch survives (Forgejo's native mirror would delete it, which
//!   is why every GitHub copy is fed by the gate alone).
//! - GitLab copy: Forgejo's native mirror deletes copy-only branches, so the head lives in a relay fork in
//!   the subgroup `<group>/cforge`; the merge request goes from the fork to the copy.
//!
//! Nothing is stored. The branch name is a pure function of the origin (forge and number), so every pass
//! reads the live state of the host (RD1) and the relay can repeat any step: a lost response, a deleted
//! branch, a deleted fork or a closed request converge on the next pass. Every write is a method of
//! [`HeadHost`] or [`HeadGit`]; there is no generic mutation path. Take-in closes every sibling (the
//! copies' requests and the origin's own request) and removes the relay branches; GitHub and GitLab mark
//! a request merged by themselves when the mirrored commits reach the base, so a request still open after
//! the mirror is closed with a note instead.

use super::forge::ForgeError;
use crate::model::Forge;
use std::{collections::BTreeMap, process::Command};

/// The relay branch of an origin request: `cforge/<origin forge>-<number>`.
pub fn branch_name(origin: Forge, number: u64) -> String {
    format!("cforge/{}-{number}", origin.as_str())
}

/// Is `branch` inside the relay namespace the gate and the heads own?
pub fn is_relay_branch(branch: &str) -> bool {
    branch.starts_with("cforge/")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReqState {
    Open,
    Merged,
    Closed,
}

/// The pull request on the forge it was opened on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    pub forge: Forge,
    /// The repository path on the origin forge.
    pub repo: String,
    pub number: u64,
    /// The head commit now.
    pub head_sha: String,
    /// Where the commits can be fetched from (git URL of the head repository).
    pub fetch_url: String,
    pub title: String,
    /// Body of the relayed request, credit and links included (rendered by the caller).
    pub body: String,
    pub state: ReqState,
}

/// A copy that shows the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Copy {
    pub forge: Forge,
    /// The copy's repository path.
    pub repo: String,
    /// The base branch of the request.
    pub base: String,
}

/// Where a head lives: the repository that holds the branch and its push URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    pub repo: String,
    pub push_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReqRef {
    pub number: u64,
    pub url: String,
    pub state: ReqState,
}

/// What the relay needs from one copy forge for heads. Reads that find nothing return `None`; a 403, a
/// 5xx or a timeout is an `Err`, never `None` (as for [`super::forge::ReadOutcome::Gone`]).
pub trait HeadHost {
    fn forge(&self) -> Forge;

    /// The place for the head of a request to `copy_repo`. GitHub: the copy itself. GitLab: the fork in
    /// `<group>/cforge`; the subgroup is looked up first and created only when it does not exist, the
    /// fork is created only when it does not exist.
    fn site(&mut self, copy_repo: &str) -> Result<Site, ForgeError>;

    /// Tip of `branch` at the site; `None` when the branch is gone.
    fn branch_tip(&mut self, site: &Site, branch: &str) -> Result<Option<String>, ForgeError>;

    /// The newest request whose head is `branch` at `site` (any state).
    fn find_request(
        &mut self,
        copy_repo: &str,
        site: &Site,
        branch: &str,
    ) -> Result<Option<ReqRef>, ForgeError>;

    fn open_request(
        &mut self,
        copy_repo: &str,
        site: &Site,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<ReqRef, ForgeError>;

    /// Current state of request `number`; `None` when it is gone.
    fn request_state(&mut self, repo: &str, number: u64) -> Result<Option<ReqState>, ForgeError>;

    /// Closes with a note. A request that is gone or already closed counts as closed.
    fn close_request(&mut self, repo: &str, number: u64, note: &str) -> Result<(), ForgeError>;

    /// A branch that is already gone counts as deleted.
    fn delete_branch(&mut self, site: &Site, branch: &str) -> Result<(), ForgeError>;
}

/// Moves commits. The pushing identity is the host's (a credential of the locked unit), never a personal one.
pub trait HeadGit {
    /// Fetches exactly `sha` from `from_url` and force-pushes it as `branch` to `to_url`.
    fn push_head(
        &mut self,
        from_url: &str,
        sha: &str,
        to_url: &str,
        branch: &str,
    ) -> Result<(), ForgeError>;
}

/// `git` on the path, for the throwaway proofs and the locked unit.
pub struct GitCli;

impl HeadGit for GitCli {
    fn push_head(
        &mut self,
        from_url: &str,
        sha: &str,
        to_url: &str,
        branch: &str,
    ) -> Result<(), ForgeError> {
        let dir = tempfile::tempdir().map_err(|e| ForgeError::Unavailable(e.to_string()))?;
        let git = |args: &[&str]| -> Result<(), ForgeError> {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .map_err(|e| ForgeError::Unavailable(e.to_string()))?;
            if out.status.success() {
                Ok(())
            } else {
                // The text never carries a URL: it may hold a credential.
                Err(ForgeError::Unavailable(format!(
                    "git {} failed",
                    args.first().copied().unwrap_or("")
                )))
            }
        };
        git(&["init", "-q", "--bare"])?;
        git(&["fetch", "-q", "--no-tags", from_url, sha])?;
        let spec = format!("+{sha}:refs/heads/{branch}");
        git(&["push", "-q", to_url, &spec])
    }
}

/// What a pass did for one copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadAction {
    /// Branch pushed and request opened.
    Created(ReqRef),
    /// The branch moved to the origin's new head.
    Updated { request: ReqRef },
    /// The branch had vanished and was pushed again (a new request when the old one died with it).
    Recreated(ReqRef),
    /// Nothing to write.
    Unchanged(ReqRef),
    /// A person closed the relayed request while the origin is open: left alone.
    LeftClosed(ReqRef),
    /// Closed because the origin closed without merging; the branch is removed.
    Retired,
    /// Take-in: the request was already merged by the mirrored commits.
    AlreadyMerged,
    /// Take-in: the request was still open and is closed (leftover).
    ClosedLeftover,
    /// Take-in: the request is gone.
    Gone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyReport {
    pub forge: Forge,
    pub repo: String,
    pub result: Result<HeadAction, ForgeError>,
}

/// The hosts a pass talks to, one per forge. A copy whose forge has no host is reported as an outage.
pub struct Heads {
    hosts: BTreeMap<Forge, Box<dyn HeadHost>>,
    git: Box<dyn HeadGit>,
}

fn outage(forge: Forge) -> ForgeError {
    ForgeError::Unavailable(format!("no head host for {}", forge.as_str()))
}

impl Heads {
    pub fn new(git: Box<dyn HeadGit>) -> Self {
        Self {
            hosts: BTreeMap::new(),
            git,
        }
    }

    pub fn insert(&mut self, host: Box<dyn HeadHost>) {
        self.hosts.insert(host.forge(), host);
    }

    /// Creates or updates the head and the request on every copy; or, when the origin closed without
    /// merging, retires them. A failing copy does not stop the others.
    pub fn sync(&mut self, origin: &Origin, copies: &[Copy]) -> Vec<CopyReport> {
        copies
            .iter()
            .filter(|c| c.forge != origin.forge)
            .map(|c| CopyReport {
                forge: c.forge,
                repo: c.repo.clone(),
                result: self.sync_one(origin, c),
            })
            .collect()
    }

    fn sync_one(&mut self, origin: &Origin, copy: &Copy) -> Result<HeadAction, ForgeError> {
        let host = self
            .hosts
            .get_mut(&copy.forge)
            .ok_or_else(|| outage(copy.forge))?;
        let branch = branch_name(origin.forge, origin.number);
        let site = host.site(&copy.repo)?;
        let found = host.find_request(&copy.repo, &site, &branch)?;
        if origin.state != ReqState::Open {
            // Closed on the origin without take-in: the head and the request go (merged ones are take-in's).
            if let Some(req) = found.filter(|r| r.state == ReqState::Open) {
                host.close_request(
                    &copy.repo,
                    req.number,
                    "The origin request was closed without merge.",
                )?;
            }
            host.delete_branch(&site, &branch)?;
            return Ok(HeadAction::Retired);
        }
        let tip = host.branch_tip(&site, &branch)?;
        if let Some(old) = found.as_ref().filter(|r| r.state != ReqState::Open) {
            // A person closed it and the branch still stands: leave both alone.
            if tip.is_some() {
                return Ok(HeadAction::LeftClosed(old.clone()));
            }
        }
        let moved = tip.as_deref() != Some(origin.head_sha.as_str());
        if moved {
            self.git
                .push_head(&origin.fetch_url, &origin.head_sha, &site.push_url, &branch)?;
        }
        let host = self
            .hosts
            .get_mut(&copy.forge)
            .ok_or_else(|| outage(copy.forge))?;
        let open = |host: &mut Box<dyn HeadHost>| {
            host.open_request(
                &copy.repo,
                &site,
                &branch,
                &copy.base,
                &origin.title,
                &origin.body,
            )
        };
        match found {
            None => Ok(HeadAction::Created(open(host)?)),
            // The branch was gone, so its request died with it (or never lived): a new one replaces it.
            Some(old) if old.state != ReqState::Open => Ok(HeadAction::Recreated(open(host)?)),
            Some(old) if tip.is_none() => Ok(HeadAction::Recreated(old)),
            Some(old) if moved => Ok(HeadAction::Updated { request: old }),
            Some(old) => Ok(HeadAction::Unchanged(old)),
        }
    }

    /// Take-in: closes every sibling as merged, after the owner pushed the reviewed commit and the
    /// mirrors carried it. A request that the forge already marked merged is left as it is; one still
    /// open is closed with a note. The relay branches go. The origin's own request is the last entry.
    pub fn take_in(
        &mut self,
        origin: &Origin,
        copies: &[Copy],
        reviewed_sha: &str,
    ) -> Vec<CopyReport> {
        let note = format!("Merged on the primary forge at {reviewed_sha}; the commits reached this repository by mirroring.");
        let mut out: Vec<CopyReport> = copies
            .iter()
            .filter(|c| c.forge != origin.forge)
            .map(|c| CopyReport {
                forge: c.forge,
                repo: c.repo.clone(),
                result: self.take_in_copy(origin, c, &note),
            })
            .collect();
        out.push(CopyReport {
            forge: origin.forge,
            repo: origin.repo.clone(),
            result: self.close_merged(origin.forge, &origin.repo, origin.number, &note),
        });
        out
    }

    fn close_merged(
        &mut self,
        forge: Forge,
        repo: &str,
        number: u64,
        note: &str,
    ) -> Result<HeadAction, ForgeError> {
        let host = self.hosts.get_mut(&forge).ok_or_else(|| outage(forge))?;
        match host.request_state(repo, number)? {
            None => Ok(HeadAction::Gone),
            Some(ReqState::Merged) => Ok(HeadAction::AlreadyMerged),
            Some(ReqState::Closed) => Ok(HeadAction::ClosedLeftover),
            Some(ReqState::Open) => {
                host.close_request(repo, number, note)?;
                Ok(HeadAction::ClosedLeftover)
            }
        }
    }

    fn take_in_copy(
        &mut self,
        origin: &Origin,
        copy: &Copy,
        note: &str,
    ) -> Result<HeadAction, ForgeError> {
        let host = self
            .hosts
            .get_mut(&copy.forge)
            .ok_or_else(|| outage(copy.forge))?;
        let branch = branch_name(origin.forge, origin.number);
        let site = host.site(&copy.repo)?;
        let action = match host.find_request(&copy.repo, &site, &branch)? {
            None => HeadAction::Gone,
            Some(req) => self.close_merged(copy.forge, &copy.repo, req.number, note)?,
        };
        let host = self
            .hosts
            .get_mut(&copy.forge)
            .ok_or_else(|| outage(copy.forge))?;
        host.delete_branch(&site, &branch)?;
        Ok(action)
    }
}
