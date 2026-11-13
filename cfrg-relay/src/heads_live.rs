//! The live GitLab `HeadHost` of the pull request heads (Bridge design §3.3, build task B8).
//!
//! Every call goes through the GitLab driver (`cglb`): the group, project, branch and merge request reads, the relay
//! fork (the `<group>/cforge` subgroup is never created here), the cross-fork merge request (`target_project_id`) and the note and close writes. There is no other HTTP
//! path here. Heads are pushed by [`cfrg::relay::heads::GitCli`] to the fork's URL.

use crate::live::{forgejo_error, forgejo_repo, github_error, gitlab_error};
use cfrg::{
    model::Forge,
    relay::{
        forge::ForgeError,
        heads::{HeadGit, HeadHost, ReqRef, ReqState, Site},
    },
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, thread, time::Duration};
use tokio::runtime::Runtime;

pub struct GitlabHeads {
    pub rt: Rc<Runtime>,
    pub driver: cglb::relay::GitlabRelay,
    /// Host for git URLs, normally `gitlab.com`.
    pub host: String,
    token: String,
    ids: RefCell<BTreeMap<String, u64>>,
}

impl GitlabHeads {
    pub fn new(
        rt: Rc<Runtime>,
        driver: cglb::relay::GitlabRelay,
        host: &str,
        token: &str,
    ) -> Result<Self, ForgeError> {
        Ok(Self {
            rt,
            driver,
            host: host.into(),
            token: token.into(),
            ids: RefCell::default(),
        })
    }

    fn path(p: &str) -> Result<cglb::relay::Path, ForgeError> {
        cglb::relay::Path::parse(p).map_err(gitlab_error)
    }

    fn project_id(&self, path: &str) -> Result<Option<u64>, ForgeError> {
        if let Some(id) = self.ids.borrow().get(path) {
            return Ok(Some(*id));
        }
        let info = self
            .rt
            .block_on(self.driver.project_info(&Self::path(path)?))
            .map_err(gitlab_error)?;
        let id = info.map(|i| i.id);
        if let Some(id) = id {
            self.ids.borrow_mut().insert(path.into(), id);
        }
        Ok(id)
    }

    fn state_of(state: &str, merged: bool) -> ReqState {
        if merged || state == "merged" {
            ReqState::Merged
        } else if state == "closed" {
            ReqState::Closed
        } else {
            ReqState::Open
        }
    }
}

impl HeadHost for GitlabHeads {
    fn forge(&self) -> Forge {
        Forge::Gitlab
    }

    fn site(&mut self, copy_repo: &str) -> Result<Site, ForgeError> {
        let (group, name) = copy_repo
            .rsplit_once('/')
            .ok_or_else(|| ForgeError::Rejected("repository path".into()))?;
        let sub = format!("{group}/cforge");
        let fork = format!("{sub}/{name}");
        // The `<group>/cforge` subgroups are made once by the owner (least privilege: the relay account is
        // Maintainer there and nowhere else). A missing one is reported, never created by the relay.
        if !self
            .rt
            .block_on(self.driver.group_exists(&Self::path(&sub)?))
            .map_err(gitlab_error)?
        {
            return Err(ForgeError::Rejected(format!(
                "the relay subgroup {sub} does not exist; the owner creates it once (the relay does not)"
            )));
        }
        if self.project_id(&fork)?.is_none() {
            let (project, group) = (Self::path(copy_repo)?, Self::path(group)?);
            self.rt
                .block_on(self.driver.fork_into_relay_subgroup(&project, &group))
                .map_err(gitlab_error)?;
            // The fork is an import: wait, bounded, until it is finished.
            for _ in 0..60 {
                let info = self
                    .rt
                    .block_on(self.driver.project_info(&Self::path(&fork)?))
                    .map_err(gitlab_error)?;
                if info.is_some_and(|i| {
                    matches!(i.import_status.as_deref(), Some("finished" | "none") | None)
                }) {
                    break;
                }
                thread::sleep(Duration::from_secs(1));
            }
            if self.project_id(&fork)?.is_none() {
                return Err(ForgeError::Unavailable(
                    "the relay fork did not appear".into(),
                ));
            }
        }
        Ok(Site {
            push_url: format!("https://oauth2:{}@{}/{fork}.git", self.token, self.host),
            repo: fork,
        })
    }

    fn branch_tip(&mut self, site: &Site, branch: &str) -> Result<Option<String>, ForgeError> {
        self.rt
            .block_on(self.driver.branch_tip(&Self::path(&site.repo)?, branch))
            .map_err(gitlab_error)
    }

    fn find_request(
        &mut self,
        copy_repo: &str,
        site: &Site,
        branch: &str,
    ) -> Result<Option<ReqRef>, ForgeError> {
        let Some(fork_id) = self.project_id(&site.repo)? else {
            return Ok(None);
        };
        let list = self
            .rt
            .block_on(
                self.driver
                    .merge_requests_from_branch(&Self::path(copy_repo)?, branch),
            )
            .map_err(gitlab_error)?;
        Ok(list
            .into_iter()
            .find(|m| m.source_project_id == fork_id)
            .map(|m| ReqRef {
                number: m.iid,
                url: m.web_url,
                state: Self::state_of(&m.state, m.merged),
            }))
    }

    fn open_request(
        &mut self,
        copy_repo: &str,
        site: &Site,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<ReqRef, ForgeError> {
        let target = self
            .project_id(copy_repo)?
            .ok_or_else(|| ForgeError::Rejected("copy not found".into()))?;
        let mr = self
            .rt
            .block_on(self.driver.merge_request_open_from_fork(
                &Self::path(&site.repo)?,
                target,
                branch,
                base,
                title,
                body,
            ))
            .map_err(gitlab_error)?;
        Ok(ReqRef {
            number: mr.iid,
            url: mr.web_url,
            state: ReqState::Open,
        })
    }

    fn request_state(&mut self, repo: &str, number: u64) -> Result<Option<ReqState>, ForgeError> {
        let mr = self
            .rt
            .block_on(self.driver.merge_request_read(&Self::path(repo)?, number))
            .map_err(gitlab_error)?;
        Ok(mr.map(|m| Self::state_of(&m.state, m.merged)))
    }

    fn close_request(&mut self, repo: &str, number: u64, note: &str) -> Result<(), ForgeError> {
        if self.request_state(repo, number)? != Some(ReqState::Open) {
            return Ok(());
        }
        let project = Self::path(repo)?;
        self.rt
            .block_on(self.driver.merge_request_note(&project, number, note))
            .map_err(gitlab_error)?;
        self.rt
            .block_on(self.driver.merge_request_close(&project, number))
            .map_err(gitlab_error)
    }

    fn delete_branch(&mut self, site: &Site, branch: &str) -> Result<(), ForgeError> {
        self.rt
            .block_on(self.driver.branch_delete(&Self::path(&site.repo)?, branch))
            .map(|_| ())
            .map_err(gitlab_error)
    }
}

/// The GitHub head host: the head is the branch `cforge/<origin>-<n>` inside the copy. Everything, including the
/// installation token for the push, goes through the `cghb` driver (no JWT or HTTP of its own).
pub struct GithubHeads {
    pub rt: Rc<Runtime>,
    pub driver: cghb::relay::GithubRelay,
}

impl GithubHeads {
    /// Builds the host. The installation token for a push is fetched per copy (the App is installed per organisation).
    pub fn new(rt: Rc<Runtime>, driver: cghb::relay::GithubRelay) -> Result<Self, ForgeError> {
        Ok(Self { rt, driver })
    }

    fn repo(r: &str) -> Result<cghb::relay::Repo, ForgeError> {
        cghb::relay::Repo::parse(r).map_err(github_error)
    }

    fn state_of(state: &str, merged: bool) -> ReqState {
        if merged {
            ReqState::Merged
        } else if state == "closed" {
            ReqState::Closed
        } else {
            ReqState::Open
        }
    }
}

impl HeadHost for GithubHeads {
    fn forge(&self) -> Forge {
        Forge::Github
    }

    fn site(&mut self, copy_repo: &str) -> Result<Site, ForgeError> {
        let repo = Self::repo(copy_repo)?;
        let token = self
            .rt
            .block_on(self.driver.installation_token(&repo))
            .map_err(github_error)?;
        Ok(Site {
            repo: copy_repo.into(),
            push_url: format!(
                "https://x-access-token:{}@github.com/{copy_repo}.git",
                token.expose()
            ),
        })
    }

    fn branch_tip(&mut self, site: &Site, branch: &str) -> Result<Option<String>, ForgeError> {
        match self
            .rt
            .block_on(self.driver.branch_head(&Self::repo(&site.repo)?, branch))
        {
            Ok(sha) => Ok(Some(sha)),
            Err(e) if matches!(e.status(), Some(404 | 410)) => Ok(None),
            Err(e) => Err(github_error(e)),
        }
    }

    fn find_request(
        &mut self,
        copy_repo: &str,
        _site: &Site,
        branch: &str,
    ) -> Result<Option<ReqRef>, ForgeError> {
        let list = self
            .rt
            .block_on(
                self.driver
                    .pulls_from_branch(&Self::repo(copy_repo)?, branch),
            )
            .map_err(github_error)?;
        Ok(list.into_iter().next().map(|p| ReqRef {
            number: p.number,
            url: p.html_url,
            state: Self::state_of(&p.state, p.merged),
        }))
    }

    fn open_request(
        &mut self,
        copy_repo: &str,
        _site: &Site,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<ReqRef, ForgeError> {
        let repo = Self::repo(copy_repo)?;
        let c = self
            .rt
            .block_on(self.driver.pull_open(&repo, title, branch, base, body))
            .map_err(github_error)?;
        Ok(ReqRef {
            number: c.number,
            url: c.html_url,
            state: ReqState::Open,
        })
    }

    fn request_state(&mut self, repo: &str, number: u64) -> Result<Option<ReqState>, ForgeError> {
        let pull = self
            .rt
            .block_on(self.driver.pull_read(&Self::repo(repo)?, number))
            .map_err(github_error)?;
        Ok(pull.map(|p| Self::state_of(&p.state, p.merged)))
    }

    fn close_request(&mut self, repo: &str, number: u64, note: &str) -> Result<(), ForgeError> {
        if self.request_state(repo, number)? != Some(ReqState::Open) {
            return Ok(());
        }
        let r = Self::repo(repo)?;
        self.rt
            .block_on(self.driver.comment(&r, number, note))
            .map_err(github_error)?;
        self.rt
            .block_on(self.driver.pull_close(&r, number))
            .map_err(github_error)
    }

    fn delete_branch(&mut self, site: &Site, branch: &str) -> Result<(), ForgeError> {
        self.rt
            .block_on(self.driver.branch_delete(&Self::repo(&site.repo)?, branch))
            .map(|_| ())
            .map_err(github_error)
    }
}

/// The Forgejo primary as the home of a request (a pull request from a fork): it is only ever an origin, so the
/// head operations are refused and only the state and the closing are real. Everything goes through `cfgj`.
pub struct ForgejoRequests {
    pub rt: Rc<Runtime>,
    pub driver: cfgj::relay::ForgejoRelay,
}

impl ForgejoRequests {
    fn number(n: u64) -> Result<i64, ForgeError> {
        i64::try_from(n).map_err(|_| ForgeError::Rejected("number out of range".into()))
    }

    fn not_a_copy() -> ForgeError {
        ForgeError::Rejected("the Forgejo primary shows a notice, never a relay head".into())
    }
}

impl HeadHost for ForgejoRequests {
    fn forge(&self) -> Forge {
        Forge::Forgejo
    }

    fn skip_reason(&self, _copy_repo: &str) -> Option<String> {
        Some("the primary is shown by the notice".into())
    }

    fn site(&mut self, _copy_repo: &str) -> Result<Site, ForgeError> {
        Err(Self::not_a_copy())
    }

    fn branch_tip(&mut self, _site: &Site, _branch: &str) -> Result<Option<String>, ForgeError> {
        Err(Self::not_a_copy())
    }

    fn find_request(
        &mut self,
        _copy_repo: &str,
        _site: &Site,
        _branch: &str,
    ) -> Result<Option<ReqRef>, ForgeError> {
        Err(Self::not_a_copy())
    }

    fn open_request(
        &mut self,
        _copy_repo: &str,
        _site: &Site,
        _branch: &str,
        _base: &str,
        _title: &str,
        _body: &str,
    ) -> Result<ReqRef, ForgeError> {
        Err(Self::not_a_copy())
    }

    fn request_state(&mut self, repo: &str, number: u64) -> Result<Option<ReqState>, ForgeError> {
        let pull = self
            .rt
            .block_on(
                self.driver
                    .pull_read(&forgejo_repo(repo)?, Self::number(number)?),
            )
            .map_err(forgejo_error)?;
        Ok(pull.map(|p| {
            if p.merged {
                ReqState::Merged
            } else if p.state == "closed" {
                ReqState::Closed
            } else {
                ReqState::Open
            }
        }))
    }

    fn close_request(&mut self, repo: &str, number: u64, note: &str) -> Result<(), ForgeError> {
        if self.request_state(repo, number)? != Some(ReqState::Open) {
            return Ok(());
        }
        let r = forgejo_repo(repo)?;
        let n = Self::number(number)?;
        self.rt
            .block_on(self.driver.comment(&r, n, note))
            .map_err(forgejo_error)?;
        self.rt
            .block_on(self.driver.pull_close(&r, n))
            .map_err(forgejo_error)
    }

    fn delete_branch(&mut self, _site: &Site, _branch: &str) -> Result<(), ForgeError> {
        Err(Self::not_a_copy())
    }
}

/// `git` with the credentials of the forge the commits come from: a private origin (a GitHub copy, a GitLab project,
/// the Forgejo primary) is fetched with the relay's own token, which never appears in a message.
pub struct LiveGit {
    pub rt: Rc<Runtime>,
    pub github: Option<cghb::relay::GithubRelay>,
    pub gitlab_token: Option<String>,
    pub forgejo_token: Option<String>,
    pub forgejo_host: String,
}

impl LiveGit {
    fn authed(&self, url: &str) -> Result<String, ForgeError> {
        let Some(rest) = url.strip_prefix("https://") else {
            return Ok(url.to_owned());
        };
        let host = rest.split('/').next().unwrap_or("");
        if host == "github.com" {
            if let Some(driver) = &self.github {
                let path = rest.split_once('/').map_or("", |(_, p)| p);
                let repo = cghb::relay::Repo::parse(
                    &path
                        .trim_end_matches('/')
                        .trim_end_matches(".git")
                        .split('/')
                        .take(2)
                        .collect::<Vec<_>>()
                        .join("/"),
                )
                .map_err(github_error)?;
                let token = self
                    .rt
                    .block_on(driver.installation_token(&repo))
                    .map_err(github_error)?;
                return Ok(format!("https://x-access-token:{}@{rest}", token.expose()));
            }
        } else if host == "gitlab.com" {
            if let Some(token) = &self.gitlab_token {
                return Ok(format!("https://oauth2:{token}@{rest}"));
            }
        } else if host == self.forgejo_host {
            if let Some(token) = &self.forgejo_token {
                return Ok(format!("https://x:{token}@{rest}"));
            }
        }
        Ok(url.to_owned())
    }
}

impl HeadGit for LiveGit {
    fn push_head(
        &mut self,
        from_url: &str,
        reference: &str,
        sha: &str,
        to_url: &str,
        branch: &str,
    ) -> Result<(), ForgeError> {
        let from = self.authed(from_url)?;
        cfrg::relay::heads::GitCli.push_head(&from, reference, sha, to_url, branch)
    }
}
