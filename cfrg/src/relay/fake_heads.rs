//! In-memory head hosts for tests: one shared world, a log of every write, and a git fake that lands
//! pushes in the world.

use super::forge::ForgeError;
use super::heads::{HeadGit, HeadHost, ReqRef, ReqState, Site};
use crate::model::Forge;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

#[derive(Default)]
pub struct World {
    pub groups: BTreeSet<String>,
    pub forks: BTreeSet<String>,
    /// (repo holding the branch, branch) -> tip
    pub branches: BTreeMap<(String, String), String>,
    /// (target repo, number) -> (site repo, branch, state)
    pub requests: BTreeMap<(String, u64), (String, String, ReqState)>,
    pub log: Vec<String>,
    /// The next write named here fails once.
    pub fail_next: Option<(String, ForgeError)>,
}

pub type Shared = Rc<RefCell<World>>;

pub struct FakeHost {
    pub forge: Forge,
    pub w: Shared,
    /// The router-side fake of the same forge and its path-to-id table, when a test reads the requests back through it.
    pub forge_world: Option<(super::fake::Handle, BTreeMap<String, String>)>,
}

impl FakeHost {
    fn check(&self, op: &str) -> Result<(), ForgeError> {
        let mut w = self.w.borrow_mut();
        if matches!(&w.fail_next, Some((n, _)) if n == op) {
            return Err(w
                .fail_next
                .take()
                .map(|(_, e)| e)
                .unwrap_or_else(|| ForgeError::Unavailable("x".into())));
        }
        Ok(())
    }
}

impl HeadHost for FakeHost {
    fn forge(&self) -> Forge {
        self.forge
    }

    fn skip_reason(&self, copy_repo: &str) -> Option<String> {
        copy_repo
            .starts_with("personal/")
            .then(|| "personal".to_owned())
    }

    fn site(&mut self, copy_repo: &str) -> Result<Site, ForgeError> {
        if self.forge == Forge::Github {
            return Ok(Site {
                repo: copy_repo.into(),
                push_url: format!("push:{copy_repo}"),
            });
        }
        let (group, name) = copy_repo
            .rsplit_once('/')
            .ok_or(ForgeError::Rejected("path".into()))?;
        let sub = format!("{group}/cforge");
        let fork = format!("{sub}/{name}");
        let mut w = self.w.borrow_mut();
        if !w.groups.contains(&sub) {
            w.log.push(format!("group_create {sub}"));
            w.groups.insert(sub);
        }
        if !w.forks.contains(&fork) {
            w.log.push(format!("fork {fork}"));
            w.forks.insert(fork.clone());
        }
        Ok(Site {
            push_url: format!("push:{fork}"),
            repo: fork,
        })
    }

    fn branch_tip(&mut self, site: &Site, branch: &str) -> Result<Option<String>, ForgeError> {
        Ok(self
            .w
            .borrow()
            .branches
            .get(&(site.repo.clone(), branch.into()))
            .cloned())
    }

    fn find_request(
        &mut self,
        copy_repo: &str,
        site: &Site,
        branch: &str,
    ) -> Result<Option<ReqRef>, ForgeError> {
        let w = self.w.borrow();
        Ok(w.requests
            .iter()
            .filter(|((r, _), (s, b, _))| r == copy_repo && *s == site.repo && b == branch)
            .max_by_key(|((_, n), _)| *n)
            .map(|((r, n), (_, _, st))| ReqRef {
                number: *n,
                url: format!("{r}!{n}"),
                state: *st,
            }))
    }

    fn open_request(
        &mut self,
        copy_repo: &str,
        site: &Site,
        branch: &str,
        base: &str,
        _t: &str,
        _b: &str,
    ) -> Result<ReqRef, ForgeError> {
        self.check("open")?;
        let mut w = self.w.borrow_mut();
        let mut n = w.requests.keys().filter(|(r, _)| r == copy_repo).count() as u64 + 1;
        if let Some((fw, ids)) = &self.forge_world {
            let sha = w
                .branches
                .get(&(site.repo.clone(), branch.into()))
                .cloned()
                .unwrap_or_default();
            let mut fw = fw.borrow_mut();
            let relay = fw.relay.clone();
            n = fw.open_pull(&ids[copy_repo], &relay, _t, _b, branch, &sha, base);
        }
        w.requests.insert(
            (copy_repo.into(), n),
            (site.repo.clone(), branch.into(), ReqState::Open),
        );
        w.log
            .push(format!("open {copy_repo}!{n} {branch} -> {base}"));
        Ok(ReqRef {
            number: n,
            url: format!("{copy_repo}!{n}"),
            state: ReqState::Open,
        })
    }

    fn request_state(&mut self, repo: &str, number: u64) -> Result<Option<ReqState>, ForgeError> {
        if let Some((fw, ids)) = &self.forge_world {
            if let Some(id) = ids.get(repo) {
                return Ok(fw
                    .borrow()
                    .pulls
                    .get(&(id.clone(), number))
                    .map(|p| p.state));
            }
        }
        Ok(self
            .w
            .borrow()
            .requests
            .get(&(repo.into(), number))
            .map(|r| r.2))
    }

    fn close_request(&mut self, repo: &str, number: u64, _note: &str) -> Result<(), ForgeError> {
        self.check("close")?;
        let mut w = self.w.borrow_mut();
        if let Some(r) = w.requests.get_mut(&(repo.into(), number)) {
            r.2 = ReqState::Closed;
        }
        w.log.push(format!("close {repo}!{number}"));
        if let Some((fw, ids)) = &self.forge_world {
            if let Some(id) = ids.get(repo) {
                fw.borrow_mut().set_pull_state(id, number, ReqState::Closed);
            }
        }
        Ok(())
    }

    fn delete_branch(&mut self, site: &Site, branch: &str) -> Result<(), ForgeError> {
        let mut w = self.w.borrow_mut();
        if w.branches
            .remove(&(site.repo.clone(), branch.into()))
            .is_some()
        {
            w.log.push(format!("delete_branch {} {branch}", site.repo));
        }
        Ok(())
    }
}

pub struct FakeGit(pub Shared);

impl HeadGit for FakeGit {
    fn push_head(
        &mut self,
        _from: &str,
        _reference: &str,
        sha: &str,
        to_url: &str,
        branch: &str,
    ) -> Result<(), ForgeError> {
        let repo = to_url
            .strip_prefix("push:")
            .ok_or(ForgeError::Rejected("url".into()))?;
        let mut w = self.0.borrow_mut();
        w.branches.insert((repo.into(), branch.into()), sha.into());
        w.log.push(format!("push {repo} {branch} {sha}"));
        Ok(())
    }
}

/// A notice host the test keeps a handle on.
#[derive(Clone, Default)]
pub struct SharedNotices(pub Rc<RefCell<super::notice::FakeNotices>>);

impl super::notice::NoticeHost for SharedNotices {
    fn find(
        &mut self,
        repo: &str,
        origin: &str,
        reference: &str,
    ) -> Result<Option<super::notice::NoticeRef>, ForgeError> {
        self.0.borrow_mut().find(repo, origin, reference)
    }
    fn create(&mut self, repo: &str, title: &str, body: &str) -> Result<u64, ForgeError> {
        self.0.borrow_mut().create(repo, title, body)
    }
    fn set_body(&mut self, repo: &str, number: u64, body: &str) -> Result<(), ForgeError> {
        self.0.borrow_mut().set_body(repo, number, body)
    }
    fn close(&mut self, repo: &str, number: u64, note: &str) -> Result<(), ForgeError> {
        self.0.borrow_mut().close(repo, number, note)
    }
}
