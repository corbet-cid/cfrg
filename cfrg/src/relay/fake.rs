//! In-memory forges for tests and dry runs. No network, no clock: every write is logged and can be
//! made to fail, to be refused, or to succeed with its answer lost (the case the intent journal exists for).

use super::forge::{
    Created, ForgeError, NewObject, ReadOutcome, Refusal, RequestRead, RouterForge,
};
use super::heads::ReqState;
use super::merge::Patch;
use super::model::{Actor, CommentState, IssueState, ObjectRef, ObjectState, Snap};
use crate::model::Forge;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
};

#[derive(Clone, Debug)]
pub struct Issue {
    pub author: Actor,
    pub snap: Snap,
    pub confidential: bool,
    pub comments: Vec<CommentState>,
    pub updated_at: u64,
    pub deleted: bool,
}

#[derive(Default)]
pub struct Repo {
    pub issues: BTreeMap<u64, Issue>,
    next_issue: u64,
    next_comment: u64,
    /// Actor ids that may delete others' posts.
    pub maintainers: BTreeSet<String>,
    /// `false` = the forge cannot say who is a maintainer (`is_maintainer` answers `None`).
    pub knows_maintainers: bool,
}

#[derive(Default)]
pub struct Faults {
    /// Every request fails as an outage.
    pub down: bool,
    /// Every request is refused.
    pub refuse: Option<Refusal>,
    /// Errors handed out one per request, oldest first.
    pub next_errors: VecDeque<ForgeError>,
    /// The next n mutations are applied, then answered with an outage (the answer is lost).
    pub lose_answers: u32,
    /// Reads answer 403 for this repository instead of the object.
    pub forbid_reads: BTreeSet<String>,
}

/// The pull request side of an issue number (forges number requests and issues in one sequence).
#[derive(Clone, Debug)]
pub struct Pull {
    pub head_sha: String,
    pub head_branch: String,
    pub base: String,
    pub state: ReqState,
}

pub struct World {
    pub pulls: BTreeMap<(String, u64), Pull>,
    pub forge: Forge,
    pub relay: Actor,
    pub repos: BTreeMap<String, Repo>,
    pub faults: Faults,
    pub log: Vec<(u64, String)>,
    /// (milliseconds on the test clock, was a mutation) per request, when a clock is attached.
    pub timeline: Vec<(u64, bool)>,
    pub clock_ms: Option<Rc<Cell<u64>>>,
    /// Shared by every fake forge of one test, so the order of requests across forges can be asserted.
    stamp: Rc<Cell<u64>>,
}

impl World {
    pub fn new(forge: Forge, relay_id: &str, stamp: Rc<Cell<u64>>) -> Self {
        Self {
            stamp,
            pulls: BTreeMap::new(),
            forge,
            relay: Actor {
                id: relay_id.to_owned(),
                login: "relay-bot".into(),
            },
            repos: BTreeMap::new(),
            faults: Faults::default(),
            log: Vec::new(),
            timeline: Vec::new(),
            clock_ms: None,
        }
    }

    /// Time is the shared sequence, so "created before" means the same on every fake forge of a test.
    fn next_tick(&mut self) -> u64 {
        let seq = self.stamp.get() + 1;
        self.stamp.set(seq);
        1_000 + seq
    }

    pub fn repo(&mut self, id: &str) -> &mut Repo {
        self.repos.entry(id.to_owned()).or_insert_with(|| Repo {
            knows_maintainers: true,
            ..Repo::default()
        })
    }

    fn url(&self, repo: &str, number: u64) -> String {
        format!(
            "https://{}.example/{repo}/issues/{number}",
            self.forge.as_str()
        )
    }

    // --- what people do on the forge (test inputs) ------------------------------------------------

    pub fn open_issue(
        &mut self,
        repo: &str,
        author: &Actor,
        title: &str,
        body: &str,
        labels: &[&str],
    ) -> u64 {
        let at = self.next_tick();
        let r = self.repo(repo);
        r.next_issue += 1;
        let number = r.next_issue;
        r.issues.insert(
            number,
            Issue {
                author: author.clone(),
                snap: Snap {
                    title: title.into(),
                    body: body.into(),
                    state: Some(IssueState::Open),
                    labels: labels.iter().map(|s| (*s).to_owned()).collect(),
                    assignees: BTreeSet::new(),
                },
                confidential: false,
                comments: Vec::new(),
                updated_at: at,
                deleted: false,
            },
        );
        number
    }

    pub fn comment(&mut self, repo: &str, number: u64, author: &Actor, body: &str) -> u64 {
        let at = self.next_tick();
        let url_base = self.url(repo, number);
        let r = self.repo(repo);
        r.next_comment += 1;
        let id = r.next_comment;
        let issue = r.issues.get_mut(&number).expect("issue exists");
        issue.comments.push(CommentState {
            id,
            url: format!("{url_base}#c{id}"),
            body: body.into(),
            author: author.clone(),
            created_at: at,
        });
        issue.updated_at = at;
        id
    }

    pub fn edit(&mut self, repo: &str, number: u64, change: impl FnOnce(&mut Snap)) {
        let at = self.next_tick();
        let issue = self
            .repo(repo)
            .issues
            .get_mut(&number)
            .expect("issue exists");
        change(&mut issue.snap);
        issue.updated_at = at;
    }

    pub fn edit_comment(&mut self, repo: &str, number: u64, id: u64, body: &str) {
        let at = self.next_tick();
        let issue = self
            .repo(repo)
            .issues
            .get_mut(&number)
            .expect("issue exists");
        issue
            .comments
            .iter_mut()
            .find(|c| c.id == id)
            .expect("comment exists")
            .body = body.into();
        issue.updated_at = at;
    }

    pub fn delete_issue(&mut self, repo: &str, number: u64) {
        self.repo(repo)
            .issues
            .get_mut(&number)
            .expect("issue exists")
            .deleted = true;
    }

    /// A pull request: an issue plus its head.
    #[allow(clippy::too_many_arguments)]
    pub fn open_pull(
        &mut self,
        repo: &str,
        author: &Actor,
        title: &str,
        body: &str,
        head_branch: &str,
        head_sha: &str,
        base: &str,
    ) -> u64 {
        let n = self.open_issue(repo, author, title, body, &[]);
        self.pulls.insert(
            (repo.to_owned(), n),
            Pull {
                head_sha: head_sha.into(),
                head_branch: head_branch.into(),
                base: base.into(),
                state: ReqState::Open,
            },
        );
        n
    }

    /// New commits on the head of a pull request.
    pub fn push_pull(&mut self, repo: &str, number: u64, sha: &str) {
        if let Some(p) = self.pulls.get_mut(&(repo.to_owned(), number)) {
            p.head_sha = sha.into();
        }
        let at = self.next_tick();
        if let Some(i) = self.repo(repo).issues.get_mut(&number) {
            i.updated_at = at;
        }
    }

    pub fn set_pull_state(&mut self, repo: &str, number: u64, state: ReqState) {
        if let Some(p) = self.pulls.get_mut(&(repo.to_owned(), number)) {
            p.state = state;
        }
        if let Some(i) = self.repo(repo).issues.get_mut(&number) {
            i.snap.state = Some(if state == ReqState::Open {
                IssueState::Open
            } else {
                IssueState::Closed
            });
        }
    }

    pub fn remove_comment(&mut self, repo: &str, number: u64, id: u64) {
        let issue = self
            .repo(repo)
            .issues
            .get_mut(&number)
            .expect("issue exists");
        issue.comments.retain(|c| c.id != id);
    }

    pub fn issue(&mut self, repo: &str, number: u64) -> Issue {
        self.repo(repo)
            .issues
            .get(&number)
            .expect("issue exists")
            .clone()
    }

    pub fn mutations(&self) -> Vec<String> {
        self.mutation_stamps()
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }

    /// Mutating requests with their cross-forge sequence number.
    pub fn mutation_stamps(&self) -> Vec<(u64, String)> {
        self.log
            .iter()
            .filter(|(_, l)| {
                !l.starts_with("read")
                    && !l.starts_with("list ")
                    && !l.starts_with("find ")
                    && !l.starts_with("maintainer ")
            })
            .cloned()
            .collect()
    }

    pub fn requests(&self) -> usize {
        self.log.len()
    }

    pub fn clear_log(&mut self) {
        self.log.clear();
    }
}

/// A handle that tests keep while the engine owns the adapter.
pub type Handle = Rc<RefCell<World>>;

pub struct FakeForge {
    world: Handle,
    forge: Forge,
}

impl FakeForge {
    pub fn new(forge: Forge, relay_id: &str, stamp: Rc<Cell<u64>>) -> (Self, Handle) {
        let world = Rc::new(RefCell::new(World::new(forge, relay_id, stamp)));
        (
            Self {
                world: Rc::clone(&world),
                forge,
            },
            world,
        )
    }

    fn gate(&self, w: &mut World, what: String, mutation: bool) -> Result<bool, ForgeError> {
        let seq = w.stamp.get() + 1;
        w.stamp.set(seq);
        w.log.push((seq, what));
        let at = w.clock_ms.as_ref().map_or(0, |c| c.get());
        w.timeline.push((at, mutation));
        if w.faults.down {
            return Err(ForgeError::Unavailable("connection refused".into()));
        }
        if let Some(kind) = w.faults.refuse {
            return Err(ForgeError::Refused(kind, "refused by the fake".into()));
        }
        if let Some(error) = w.faults.next_errors.pop_front() {
            return Err(error);
        }
        if mutation && w.faults.lose_answers > 0 {
            w.faults.lose_answers -= 1;
            return Ok(true);
        }
        Ok(false)
    }

    fn lost() -> ForgeError {
        ForgeError::Unavailable("the answer was lost".into())
    }

    fn state_of(w: &mut World, repo: &str, number: u64) -> Option<ObjectState> {
        let url = w.url(repo, number);
        let issue = w.repos.get(repo)?.issues.get(&number)?;
        (!issue.deleted).then(|| ObjectState {
            number,
            url,
            author: issue.author.clone(),
            snap: issue.snap.clone(),
            confidential: issue.confidential,
            comments: issue.comments.clone(),
            updated_at: issue.updated_at,
        })
    }
}

impl RouterForge for FakeForge {
    fn forge(&self) -> Forge {
        self.forge
    }

    fn read_request(&mut self, repo: &str, number: u64) -> Result<Option<RequestRead>, ForgeError> {
        let world = Rc::clone(&self.world);
        let mut w = world.borrow_mut();
        self.gate(&mut w, format!("read-request {repo} #{number}"), false)?;
        let Some(pull) = w.pulls.get(&(repo.to_owned(), number)).cloned() else {
            return Ok(None);
        };
        let Some(state) = Self::state_of(&mut w, repo, number) else {
            return Ok(None);
        };
        let forge = w.forge.as_str();
        let reference = if w.forge == Forge::Gitlab {
            format!("refs/merge-requests/{number}/head")
        } else {
            format!("refs/pull/{number}/head")
        };
        Ok(Some(RequestRead {
            number,
            url: format!("https://{forge}.example/{repo}/pull/{number}"),
            author: state.author,
            title: state.snap.title,
            body: state.snap.body,
            state: pull.state,
            head_sha: pull.head_sha,
            head_branch: pull.head_branch,
            base: pull.base,
            fetch_url: format!("https://{forge}.example/{repo}.git"),
            reference,
            comments: state.comments,
            updated_at: state.updated_at,
        }))
    }

    fn read(&mut self, object: &ObjectRef) -> Result<ReadOutcome, ForgeError> {
        let world = Rc::clone(&self.world);
        let mut w = world.borrow_mut();
        self.gate(
            &mut w,
            format!("read {} #{}", object.repo, object.number),
            false,
        )?;
        if w.faults.forbid_reads.contains(&object.repo) {
            return Err(ForgeError::Refused(
                Refusal::Forbidden,
                "reads are forbidden".into(),
            ));
        }
        Ok(match Self::state_of(&mut w, &object.repo, object.number) {
            Some(state) => ReadOutcome::Found(Box::new(state)),
            None => ReadOutcome::Gone,
        })
    }

    fn create_object(&mut self, repo: &str, new: &NewObject) -> Result<Created, ForgeError> {
        let world = Rc::clone(&self.world);
        let mut w = world.borrow_mut();
        let lose = self.gate(&mut w, format!("create {repo}: {}", new.title), true)?;
        let relay = w.relay.clone();
        let labels: Vec<&str> = new.labels.iter().map(String::as_str).collect();
        let number = w.open_issue(repo, &relay, &new.title, &new.body, &labels);
        if lose {
            return Err(Self::lost());
        }
        let url = w.url(repo, number);
        let snap = w.issue(repo, number).snap;
        Ok(Created { number, url, snap })
    }

    fn update_object(&mut self, object: &ObjectRef, patch: &Patch) -> Result<(), ForgeError> {
        let world = Rc::clone(&self.world);
        let mut w = world.borrow_mut();
        let lose = self.gate(
            &mut w,
            format!("update {} #{}", object.repo, object.number),
            true,
        )?;
        let at = w.next_tick();
        let issue = w
            .repos
            .get_mut(&object.repo)
            .and_then(|r| r.issues.get_mut(&object.number))
            .filter(|i| !i.deleted)
            .ok_or_else(|| ForgeError::Rejected("404 not found".into()))?;
        patch.apply(&mut issue.snap);
        issue.updated_at = at;
        if lose {
            return Err(Self::lost());
        }
        Ok(())
    }

    fn create_comment(&mut self, object: &ObjectRef, body: &str) -> Result<u64, ForgeError> {
        let world = Rc::clone(&self.world);
        let mut w = world.borrow_mut();
        let lose = self.gate(
            &mut w,
            format!("comment {} #{}", object.repo, object.number),
            true,
        )?;
        if !w
            .repos
            .get(&object.repo)
            .is_some_and(|r| r.issues.get(&object.number).is_some_and(|i| !i.deleted))
        {
            return Err(ForgeError::Rejected("404 not found".into()));
        }
        let relay = w.relay.clone();
        let id = w.comment(&object.repo, object.number, &relay, body);
        if lose {
            return Err(Self::lost());
        }
        Ok(id)
    }

    fn update_comment(
        &mut self,
        object: &ObjectRef,
        id: u64,
        body: &str,
    ) -> Result<(), ForgeError> {
        let world = Rc::clone(&self.world);
        let mut w = world.borrow_mut();
        let lose = self.gate(
            &mut w,
            format!("edit-comment {} #{} {id}", object.repo, object.number),
            true,
        )?;
        let exists = w
            .repos
            .get(&object.repo)
            .and_then(|r| r.issues.get(&object.number))
            .is_some_and(|i| i.comments.iter().any(|c| c.id == id));
        if !exists {
            return Err(ForgeError::Rejected("404 not found".into()));
        }
        w.edit_comment(&object.repo, object.number, id, body);
        if lose {
            return Err(Self::lost());
        }
        Ok(())
    }

    fn delete_comment(&mut self, object: &ObjectRef, id: u64) -> Result<(), ForgeError> {
        let world = Rc::clone(&self.world);
        let mut w = world.borrow_mut();
        let lose = self.gate(
            &mut w,
            format!("delete-comment {} #{} {id}", object.repo, object.number),
            true,
        )?;
        if let Some(issue) = w
            .repos
            .get_mut(&object.repo)
            .and_then(|r| r.issues.get_mut(&object.number))
        {
            issue.comments.retain(|c| c.id != id);
        }
        if lose {
            return Err(Self::lost());
        }
        Ok(())
    }

    fn delete_object(&mut self, object: &ObjectRef) -> Result<(), ForgeError> {
        let world = Rc::clone(&self.world);
        let mut w = world.borrow_mut();
        let lose = self.gate(
            &mut w,
            format!("delete {} #{}", object.repo, object.number),
            true,
        )?;
        if let Some(issue) = w
            .repos
            .get_mut(&object.repo)
            .and_then(|r| r.issues.get_mut(&object.number))
        {
            issue.deleted = true;
        }
        if lose {
            return Err(Self::lost());
        }
        Ok(())
    }

    fn find_by_marker(&mut self, repo: &str, marker: &str) -> Result<Option<Created>, ForgeError> {
        let world = Rc::clone(&self.world);
        let mut w = world.borrow_mut();
        self.gate(&mut w, format!("find {repo}"), false)?;
        let relay = w.relay.clone();
        let found = w.repos.get(repo).and_then(|r| {
            r.issues.iter().find(|(_, i)| {
                !i.deleted && i.author == relay && i.snap.body.lines().any(|l| l.trim() == marker)
            })
        });
        let Some((number, issue)) = found.map(|(n, i)| (*n, i.clone())) else {
            return Ok(None);
        };
        Ok(Some(Created {
            number,
            url: w.url(repo, number),
            snap: issue.snap,
        }))
    }

    fn is_maintainer(&mut self, repo: &str, actor: &Actor) -> Result<Option<bool>, ForgeError> {
        let world = Rc::clone(&self.world);
        let mut w = world.borrow_mut();
        self.gate(&mut w, format!("maintainer {repo} {}", actor.id), false)?;
        let r = w.repo(repo);
        Ok(r.knows_maintainers
            .then(|| r.maintainers.contains(&actor.id)))
    }
}
