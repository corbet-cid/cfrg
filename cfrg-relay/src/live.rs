//! The live forges of `cfrg relay`: the credentials, the one runtime, and the adapters of the three B3 drivers.
//!
//! Credentials come ONLY from systemd credentials (`$CREDENTIALS_DIRECTORY/<name>`, names in `docs/relay.md`); there is
//! no environment variable, no file path argument and no fallback to any personal token. A forge whose credential is
//! missing or unreadable gets no adapter: the engine treats it as an outage (RF1), and one log line on standard error
//! names the forge and the credential file (never a value).
//!
//! The drivers implement the whole `RouterForge` trait (build tasks B3 and B3b): reads, edits, deletions, the marker
//! search and the maintainer lookup. A status stays a status: 401/403/429/402 are refusals (they hold a namespace),
//! 5xx and transport failures are outages, other 4xx are rejections, and only a definitive 404/410 is `Gone`.
use cfrg::{
    model::Forge,
    relay::{
        forge::{
            from_status, has_marker_line, Created, ForgeError, NewObject, ReadOutcome, RequestRead,
            RouterForge,
        },
        heads::ReqState,
        merge::Patch,
        model::{Actor, IssueState, ObjectRef, RouterConfig},
        Forges,
    },
};
use std::{cell::RefCell, collections::BTreeMap, fs, path::PathBuf, rc::Rc};
use tokio::runtime::Runtime;

/// The fixed credential names (files of the credentials directory).
pub const FORGEJO_TOKEN: &str = "forgejo-token";
pub const GITLAB_TOKEN: &str = "gitlab-token";
pub const GITHUB_APP_ID: &str = "github-app-id";
pub const GITHUB_INSTALLATION_ID: &str = "github-installation-id";
pub const GITHUB_APP_KEY: &str = "github-app-key";

/// The directory used when none is configured: `$CREDENTIALS_DIRECTORY` (a systemd unit), else none.
pub fn default_credentials_dir() -> Option<PathBuf> {
    std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from)
}

/// Reads one credential; `None` for a missing, unreadable or empty one. Never a value in a message.
pub fn credential(dir: &Option<PathBuf>, name: &str) -> Option<String> {
    let dir = dir.as_ref()?;
    let text = fs::read_to_string(dir.join(name)).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn refused(forge: &str, names: &[&str]) {
    eprintln!(
        "relay: {forge} refused: credential {} is not in the credentials directory; no request is made to {forge}",
        names.join(", ")
    );
}

/// The one small current-thread runtime of the process.
pub fn runtime() -> cfrg::Result<Rc<Runtime>> {
    Ok(Rc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?,
    ))
}

/// Repositories a definitive 404 was seen on, as (forge, repository id): the next run resolves their ids again.
pub type Stale = Rc<RefCell<Vec<(Forge, String)>>>;

fn classify(status: Option<u16>, invalid: bool, text: String) -> ForgeError {
    match (status, invalid) {
        (Some(code), _) => from_status(code, text),
        (None, true) => ForgeError::Rejected(text),
        (None, false) => ForgeError::Unavailable(text),
    }
}

pub fn github_error(e: cghb::relay::Error) -> ForgeError {
    classify(
        e.status(),
        matches!(e, cghb::relay::Error::Invalid(_)),
        e.to_string(),
    )
}

pub fn gitlab_error(e: cglb::relay::Error) -> ForgeError {
    classify(
        e.status(),
        matches!(e, cglb::relay::Error::Invalid(_)),
        e.to_string(),
    )
}

pub fn forgejo_error(e: cfgj::relay::Error) -> ForgeError {
    classify(
        e.status(),
        matches!(e, cfgj::relay::Error::Invalid(_)),
        e.to_string(),
    )
}

pub fn forgejo_repo(path: &str) -> Result<cfgj::relay::Repo, ForgeError> {
    cfgj::relay::Repo::parse(path).map_err(forgejo_error)
}

/// The authenticated drivers of the forges whose credential is present.
pub struct Drivers {
    github: Option<cghb::relay::GithubRelay>,
    gitlab: Option<cglb::relay::GitlabRelay>,
    forgejo: Option<cfgj::relay::ForgejoRelay>,
    forgejo_origin: String,
}

pub fn open_forgejo(dir: &Option<PathBuf>, origin: &str) -> Option<cfgj::relay::ForgejoRelay> {
    match credential(dir, FORGEJO_TOKEN) {
        Some(token) => {
            match cfgj::relay::ForgejoRelay::new(origin, &cfgj::relay::Token::new(token)) {
                Ok(driver) => Some(driver),
                Err(why) => {
                    eprintln!("relay: forgejo refused: {why}");
                    None
                }
            }
        }
        None => {
            refused("forgejo", &[FORGEJO_TOKEN]);
            None
        }
    }
}

pub fn open_gitlab(dir: &Option<PathBuf>, rt: &Rc<Runtime>) -> Option<cglb::relay::GitlabRelay> {
    match credential(dir, GITLAB_TOKEN) {
        Some(token) => match rt.block_on(cglb::relay::GitlabRelay::new(
            "gitlab.com",
            &cglb::relay::Token::new(token),
            false,
        )) {
            Ok(driver) => Some(driver),
            Err(why) => {
                eprintln!("relay: gitlab refused: {why}");
                None
            }
        },
        None => {
            refused("gitlab", &[GITLAB_TOKEN]);
            None
        }
    }
}

pub fn open_github(dir: &Option<PathBuf>, rt: &Rc<Runtime>) -> Option<cghb::relay::GithubRelay> {
    let app = (
        credential(dir, GITHUB_APP_ID),
        credential(dir, GITHUB_INSTALLATION_ID),
        credential(dir, GITHUB_APP_KEY),
    );
    match app {
        (Some(app_id), Some(installation_id), Some(key)) => {
            match (app_id.parse::<u64>(), installation_id.parse::<u64>()) {
                (Ok(app_id), Ok(installation_id)) => {
                    let creds = cghb::relay::AppCredentials {
                        app_id,
                        installation_id,
                        private_key_pem: cghb::relay::Redacted::new(key),
                    };
                    match rt.block_on(async {
                        cghb::relay::GithubRelay::new(&creds, "https://api.github.com")
                    }) {
                        Ok(driver) => Some(driver),
                        Err(why) => {
                            eprintln!("relay: github refused: {why}");
                            None
                        }
                    }
                }
                _ => {
                    eprintln!("relay: github refused: {GITHUB_APP_ID} and {GITHUB_INSTALLATION_ID} must be numbers");
                    None
                }
            }
        }
        _ => {
            refused(
                "github",
                &[GITHUB_APP_ID, GITHUB_INSTALLATION_ID, GITHUB_APP_KEY],
            );
            None
        }
    }
}

impl Drivers {
    pub fn open(dir: &Option<PathBuf>, rt: &Rc<Runtime>, forgejo_origin: &str) -> Self {
        Self {
            forgejo: open_forgejo(dir, forgejo_origin),
            gitlab: open_gitlab(dir, rt),
            github: open_github(dir, rt),
            forgejo_origin: forgejo_origin.to_owned(),
        }
    }

    /// The immutable repository id of a copy, from its path. `Ok(None)` is a definitive 404/410 (no such copy);
    /// a forge without a driver is an outage.
    pub fn resolve(
        &self,
        rt: &Runtime,
        forge: Forge,
        path: &str,
    ) -> Result<Option<String>, ForgeError> {
        let no_driver = || ForgeError::Unavailable(format!("no {} credential", forge.as_str()));
        match forge {
            Forge::Github => {
                let d = self.github.as_ref().ok_or_else(no_driver)?;
                let repo = cghb::relay::Repo::parse(path).map_err(github_error)?;
                Ok(rt
                    .block_on(d.repo_id(&repo))
                    .map_err(github_error)?
                    .map(|i| i.to_string()))
            }
            Forge::Gitlab => {
                let d = self.gitlab.as_ref().ok_or_else(no_driver)?;
                let p = cglb::relay::Path::parse(path).map_err(gitlab_error)?;
                Ok(rt
                    .block_on(d.project_id(&p))
                    .map_err(gitlab_error)?
                    .map(|i| i.to_string()))
            }
            Forge::Forgejo => {
                let d = self.forgejo.as_ref().ok_or_else(no_driver)?;
                Ok(rt
                    .block_on(d.repo_id(&forgejo_repo(path)?))
                    .map_err(forgejo_error)?
                    .map(|i| i.to_string()))
            }
            Forge::Bitbucket => Err(ForgeError::Rejected(
                "Bitbucket is a git mirror only".into(),
            )),
        }
    }

    /// The router adapters of the drivers that exist.
    pub fn into_forges(self, rt: &Rc<Runtime>, config: &RouterConfig, stale: &Stale) -> Forges {
        // repository id -> path, per forge: the engine names repositories by immutable id, the drivers by path.
        let mut paths: BTreeMap<Forge, BTreeMap<String, String>> = BTreeMap::new();
        for set in &config.repos {
            for (forge, member) in &set.members {
                paths
                    .entry(*forge)
                    .or_default()
                    .insert(member.id.clone(), member.path.clone());
            }
        }
        let paths_of = |forge: Forge| paths.get(&forge).cloned().unwrap_or_default();
        let relay_of = |forge: Forge| config.relay.get(&forge).cloned().unwrap_or_default();
        let mut forges = Forges::new();
        if let Some(driver) = self.forgejo {
            forges.insert(Box::new(Forgejo {
                rt: rt.clone(),
                driver,
                paths: paths_of(Forge::Forgejo),
                relay: relay_of(Forge::Forgejo),
                stale: stale.clone(),
                origin: self.forgejo_origin.clone(),
            }));
        }
        if let Some(driver) = self.gitlab {
            forges.insert(Box::new(Gitlab {
                rt: rt.clone(),
                driver,
                paths: paths_of(Forge::Gitlab),
                relay: relay_of(Forge::Gitlab),
                stale: stale.clone(),
            }));
        }
        if let Some(driver) = self.github {
            forges.insert(Box::new(Github {
                rt: rt.clone(),
                driver,
                paths: paths_of(Forge::Github),
                relay: relay_of(Forge::Github),
                stale: stale.clone(),
            }));
        }
        forges
    }
}

/// Adapters for every forge whose credential is present.
pub fn forges(
    dir: &Option<PathBuf>,
    rt: &Rc<Runtime>,
    config: &RouterConfig,
    forgejo_origin: &str,
) -> Forges {
    Drivers::open(dir, rt, forgejo_origin).into_forges(rt, config, &Stale::default())
}

fn path_of(paths: &BTreeMap<String, String>, repo: &str) -> Result<String, ForgeError> {
    paths.get(repo).cloned().ok_or_else(|| {
        ForgeError::Rejected(format!(
            "repository {repo} is not a member of the relay set"
        ))
    })
}

fn snap_of(new: &NewObject) -> cfrg::relay::model::Snap {
    cfrg::relay::model::Snap {
        title: new.title.clone(),
        body: new.body.clone(),
        state: Some(IssueState::Open),
        labels: new.labels.iter().cloned().collect(),
        assignees: Default::default(),
    }
}

/// The relay-authored object carrying `marker` among recent issues, if any (lost-response recovery).
fn marker_match(
    recent: Vec<cfrg::relay::model::ObjectState>,
    relay: &[String],
    marker: &str,
) -> Option<Created> {
    recent
        .into_iter()
        .find(|o| relay.contains(&o.author.id) && has_marker_line(&o.snap.body, marker))
        .map(|o| Created {
            number: o.number,
            url: o.url,
            snap: o.snap,
        })
}

fn found(
    forge: Forge,
    stale: &Stale,
    repo: &str,
    state: Option<cfrg::relay::model::ObjectState>,
) -> ReadOutcome {
    match state {
        Some(o) => ReadOutcome::Found(Box::new(o)),
        None => {
            // Re-resolve the repository's id on the next run, only after a definitive 404/410.
            stale.borrow_mut().push((forge, repo.to_owned()));
            ReadOutcome::Gone
        }
    }
}

fn number_i64(n: u64) -> Result<i64, ForgeError> {
    i64::try_from(n).map_err(|_| ForgeError::Rejected("number out of range".into()))
}

fn req_state(state: &str, merged: bool) -> ReqState {
    if merged || state == "merged" {
        ReqState::Merged
    } else if state == "closed" {
        ReqState::Closed
    } else {
        ReqState::Open
    }
}

/// A pull or merge request from its issue-shaped read (title, text, author, comments) and its head facts.
#[allow(clippy::too_many_arguments)]
fn request_of(
    issue: cfrg::relay::model::ObjectState,
    url: String,
    state: ReqState,
    head_sha: Option<String>,
    head_branch: String,
    base: String,
    fetch_url: String,
    reference: String,
) -> Result<RequestRead, ForgeError> {
    Ok(RequestRead {
        number: issue.number,
        url,
        author: issue.author,
        title: issue.snap.title,
        body: issue.snap.body,
        state,
        head_sha: head_sha
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ForgeError::Unavailable("the request has no head commit yet".into()))?,
        head_branch,
        base,
        fetch_url,
        reference,
        comments: issue.comments,
        updated_at: issue.updated_at,
    })
}

struct Github {
    rt: Rc<Runtime>,
    driver: cghb::relay::GithubRelay,
    paths: BTreeMap<String, String>,
    relay: Vec<String>,
    stale: Stale,
}
impl Github {
    fn repo(&self, id: &str) -> Result<cghb::relay::Repo, ForgeError> {
        cghb::relay::Repo::parse(&path_of(&self.paths, id)?).map_err(github_error)
    }
}
impl RouterForge for Github {
    fn forge(&self) -> Forge {
        Forge::Github
    }
    fn read_request(&mut self, repo: &str, number: u64) -> Result<Option<RequestRead>, ForgeError> {
        let r = self.repo(repo)?;
        let Some(pull) = self
            .rt
            .block_on(self.driver.pull_read(&r, number))
            .map_err(github_error)?
        else {
            return Ok(None);
        };
        let Some(issue) = self
            .rt
            .block_on(self.driver.pull_conversation(&r, number))
            .map_err(github_error)?
        else {
            return Ok(None);
        };
        let path = path_of(&self.paths, repo)?;
        request_of(
            issue,
            pull.html_url,
            req_state(&pull.state, pull.merged),
            Some(pull.head_sha),
            pull.head_ref,
            pull.base_ref,
            format!("https://github.com/{path}.git"),
            format!("refs/pull/{number}/head"),
        )
        .map(Some)
    }
    fn read(&mut self, o: &ObjectRef) -> Result<ReadOutcome, ForgeError> {
        let r = self.repo(&o.repo)?;
        let state = self
            .rt
            .block_on(self.driver.issue_read(&r, o.number))
            .map_err(github_error)?;
        Ok(found(Forge::Github, &self.stale, &o.repo, state))
    }
    fn create_object(&mut self, repo: &str, new: &NewObject) -> Result<Created, ForgeError> {
        let r = self.repo(repo)?;
        let c = self
            .rt
            .block_on(
                self.driver
                    .issue_create(&r, &new.title, &new.body, &new.labels),
            )
            .map_err(github_error)?;
        Ok(Created {
            number: c.number,
            url: c.html_url,
            snap: snap_of(new),
        })
    }
    fn update_object(&mut self, o: &ObjectRef, patch: &Patch) -> Result<(), ForgeError> {
        if patch.is_empty() {
            return Ok(());
        }
        let r = self.repo(&o.repo)?;
        self.rt
            .block_on(self.driver.issue_edit(&r, o.number, patch))
            .map_err(github_error)
    }
    fn create_comment(&mut self, o: &ObjectRef, body: &str) -> Result<u64, ForgeError> {
        let r = self.repo(&o.repo)?;
        Ok(self
            .rt
            .block_on(self.driver.comment(&r, o.number, body))
            .map_err(github_error)?
            .id)
    }
    fn update_comment(&mut self, o: &ObjectRef, id: u64, body: &str) -> Result<(), ForgeError> {
        let r = self.repo(&o.repo)?;
        self.rt
            .block_on(self.driver.comment_edit(&r, id, body))
            .map_err(github_error)
    }
    fn delete_comment(&mut self, o: &ObjectRef, id: u64) -> Result<(), ForgeError> {
        let r = self.repo(&o.repo)?;
        self.rt
            .block_on(self.driver.comment_delete(&r, id))
            .map(|_| ())
            .map_err(github_error)
    }
    fn delete_object(&mut self, o: &ObjectRef) -> Result<(), ForgeError> {
        let r = self.repo(&o.repo)?;
        self.rt
            .block_on(self.driver.issue_delete(&r, o.number))
            .map(|_| ())
            .map_err(github_error)
    }
    fn find_by_marker(&mut self, repo: &str, marker: &str) -> Result<Option<Created>, ForgeError> {
        let r = self.repo(repo)?;
        match self.rt.block_on(self.driver.issues_recent(&r)) {
            Ok(recent) => Ok(marker_match(recent, &self.relay, marker)),
            Err(e) => {
                if e.status() == Some(404) {
                    self.stale
                        .borrow_mut()
                        .push((Forge::Github, repo.to_owned()));
                }
                Err(github_error(e))
            }
        }
    }
    fn is_maintainer(&mut self, repo: &str, actor: &Actor) -> Result<Option<bool>, ForgeError> {
        let r = self.repo(repo)?;
        self.rt
            .block_on(self.driver.is_maintainer(&r, &actor.login))
            .map(Some)
            .map_err(github_error)
    }
}

struct Gitlab {
    rt: Rc<Runtime>,
    driver: cglb::relay::GitlabRelay,
    paths: BTreeMap<String, String>,
    relay: Vec<String>,
    stale: Stale,
}
impl Gitlab {
    fn project(&self, id: &str) -> Result<cglb::relay::Path, ForgeError> {
        cglb::relay::Path::parse(&path_of(&self.paths, id)?).map_err(gitlab_error)
    }
}
impl RouterForge for Gitlab {
    fn forge(&self) -> Forge {
        Forge::Gitlab
    }
    fn read_request(&mut self, repo: &str, number: u64) -> Result<Option<RequestRead>, ForgeError> {
        let p = self.project(repo)?;
        let Some((mr, issue)) = self
            .rt
            .block_on(self.driver.merge_request_object(&p, number))
            .map_err(gitlab_error)?
        else {
            return Ok(None);
        };
        let path = path_of(&self.paths, repo)?;
        request_of(
            issue,
            mr.web_url,
            req_state(&mr.state, mr.merged),
            mr.head_sha,
            mr.source_branch,
            mr.target_branch,
            format!("https://gitlab.com/{path}.git"),
            format!("refs/merge-requests/{number}/head"),
        )
        .map(Some)
    }
    fn read(&mut self, o: &ObjectRef) -> Result<ReadOutcome, ForgeError> {
        let p = self.project(&o.repo)?;
        let state = self
            .rt
            .block_on(self.driver.issue_read(&p, o.number))
            .map_err(gitlab_error)?;
        Ok(found(Forge::Gitlab, &self.stale, &o.repo, state))
    }
    fn create_object(&mut self, repo: &str, new: &NewObject) -> Result<Created, ForgeError> {
        let p = self.project(repo)?;
        let c = self
            .rt
            .block_on(
                self.driver
                    .issue_create(&p, &new.title, &new.body, &new.labels),
            )
            .map_err(gitlab_error)?;
        Ok(Created {
            number: c.iid,
            url: c.web_url,
            snap: snap_of(new),
        })
    }
    fn update_object(&mut self, o: &ObjectRef, patch: &Patch) -> Result<(), ForgeError> {
        if patch.is_empty() {
            return Ok(());
        }
        let p = self.project(&o.repo)?;
        self.rt
            .block_on(self.driver.issue_edit(&p, o.number, patch))
            .map_err(gitlab_error)
    }
    fn create_comment(&mut self, o: &ObjectRef, body: &str) -> Result<u64, ForgeError> {
        let p = self.project(&o.repo)?;
        if o.kind == cfrg::relay::model::Kind::Contribution {
            return Ok(self
                .rt
                .block_on(self.driver.merge_request_note(&p, o.number, body))
                .map_err(gitlab_error)?
                .id);
        }
        Ok(self
            .rt
            .block_on(self.driver.issue_note(&p, o.number, body))
            .map_err(gitlab_error)?
            .id)
    }
    fn update_comment(&mut self, o: &ObjectRef, id: u64, body: &str) -> Result<(), ForgeError> {
        let p = self.project(&o.repo)?;
        self.rt
            .block_on(self.driver.note_edit(&p, o.number, id, body))
            .map_err(gitlab_error)
    }
    fn delete_comment(&mut self, o: &ObjectRef, id: u64) -> Result<(), ForgeError> {
        let p = self.project(&o.repo)?;
        self.rt
            .block_on(self.driver.note_delete(&p, o.number, id))
            .map(|_| ())
            .map_err(gitlab_error)
    }
    fn delete_object(&mut self, o: &ObjectRef) -> Result<(), ForgeError> {
        let p = self.project(&o.repo)?;
        self.rt
            .block_on(self.driver.issue_delete(&p, o.number))
            .map(|_| ())
            .map_err(gitlab_error)
    }
    fn find_by_marker(&mut self, repo: &str, marker: &str) -> Result<Option<Created>, ForgeError> {
        let p = self.project(repo)?;
        match self.rt.block_on(self.driver.issues_recent(&p)) {
            Ok(recent) => Ok(marker_match(recent, &self.relay, marker)),
            Err(e) => {
                if e.status() == Some(404) {
                    self.stale
                        .borrow_mut()
                        .push((Forge::Gitlab, repo.to_owned()));
                }
                Err(gitlab_error(e))
            }
        }
    }
    fn is_maintainer(&mut self, repo: &str, actor: &Actor) -> Result<Option<bool>, ForgeError> {
        let Ok(user) = actor.id.parse::<u64>() else {
            return Ok(None);
        };
        let p = self.project(repo)?;
        self.rt
            .block_on(self.driver.is_maintainer(&p, user))
            .map(Some)
            .map_err(gitlab_error)
    }
}

struct Forgejo {
    rt: Rc<Runtime>,
    driver: cfgj::relay::ForgejoRelay,
    paths: BTreeMap<String, String>,
    relay: Vec<String>,
    stale: Stale,
    /// Web origin of the instance, for the clone URL of a request head.
    origin: String,
}
impl Forgejo {
    fn repo(&self, id: &str) -> Result<cfgj::relay::Repo, ForgeError> {
        forgejo_repo(&path_of(&self.paths, id)?)
    }
}
impl RouterForge for Forgejo {
    fn forge(&self) -> Forge {
        Forge::Forgejo
    }
    fn read_request(&mut self, repo: &str, number: u64) -> Result<Option<RequestRead>, ForgeError> {
        let r = self.repo(repo)?;
        let n = number_i64(number)?;
        let Some(pull) = self
            .rt
            .block_on(self.driver.pull_read(&r, n))
            .map_err(forgejo_error)?
        else {
            return Ok(None);
        };
        let Some(issue) = self
            .rt
            .block_on(self.driver.pull_conversation(&r, n))
            .map_err(forgejo_error)?
        else {
            return Ok(None);
        };
        let path = path_of(&self.paths, repo)?;
        request_of(
            issue,
            pull.html_url,
            req_state(&pull.state, pull.merged),
            pull.head_sha,
            pull.head_ref,
            pull.base_ref,
            format!("{}/{path}.git", self.origin.trim_end_matches('/')),
            format!("refs/pull/{number}/head"),
        )
        .map(Some)
    }
    fn read(&mut self, o: &ObjectRef) -> Result<ReadOutcome, ForgeError> {
        let r = self.repo(&o.repo)?;
        let state = self
            .rt
            .block_on(self.driver.issue_read(&r, number_i64(o.number)?))
            .map_err(forgejo_error)?;
        Ok(found(Forge::Forgejo, &self.stale, &o.repo, state))
    }
    fn create_object(&mut self, repo: &str, new: &NewObject) -> Result<Created, ForgeError> {
        let r = self.repo(repo)?;
        let c = self
            .rt
            .block_on(self.driver.issue_create(&r, &new.title, &new.body))
            .map_err(forgejo_error)?;
        if !new.labels.is_empty() {
            self.rt
                .block_on(self.driver.labels_ensure(&r, &new.labels))
                .map_err(forgejo_error)?;
            self.rt
                .block_on(self.driver.labels_add(&r, c.number, &new.labels))
                .map_err(forgejo_error)?;
        }
        Ok(Created {
            number: c.number as u64,
            url: c.html_url,
            snap: snap_of(new),
        })
    }
    fn update_object(&mut self, o: &ObjectRef, patch: &Patch) -> Result<(), ForgeError> {
        if patch.is_empty() {
            return Ok(());
        }
        let r = self.repo(&o.repo)?;
        self.rt
            .block_on(self.driver.issue_edit(&r, number_i64(o.number)?, patch))
            .map_err(forgejo_error)
    }
    fn create_comment(&mut self, o: &ObjectRef, body: &str) -> Result<u64, ForgeError> {
        let r = self.repo(&o.repo)?;
        let id = self
            .rt
            .block_on(self.driver.comment(&r, number_i64(o.number)?, body))
            .map_err(forgejo_error)?;
        u64::try_from(id).map_err(|_| ForgeError::Rejected("negative comment id".into()))
    }
    fn update_comment(&mut self, o: &ObjectRef, id: u64, body: &str) -> Result<(), ForgeError> {
        let r = self.repo(&o.repo)?;
        self.rt
            .block_on(self.driver.comment_edit(&r, number_i64(id)?, body))
            .map_err(forgejo_error)
    }
    fn delete_comment(&mut self, o: &ObjectRef, id: u64) -> Result<(), ForgeError> {
        let r = self.repo(&o.repo)?;
        self.rt
            .block_on(self.driver.comment_delete(&r, number_i64(id)?))
            .map(|_| ())
            .map_err(forgejo_error)
    }
    fn delete_object(&mut self, o: &ObjectRef) -> Result<(), ForgeError> {
        let r = self.repo(&o.repo)?;
        self.rt
            .block_on(self.driver.issue_delete(&r, number_i64(o.number)?))
            .map(|_| ())
            .map_err(forgejo_error)
    }
    fn find_by_marker(&mut self, repo: &str, marker: &str) -> Result<Option<Created>, ForgeError> {
        let r = self.repo(repo)?;
        match self.rt.block_on(self.driver.issues_recent(&r)) {
            Ok(recent) => Ok(marker_match(recent, &self.relay, marker)),
            Err(e) => {
                if e.status() == Some(404) {
                    self.stale
                        .borrow_mut()
                        .push((Forge::Forgejo, repo.to_owned()));
                }
                Err(forgejo_error(e))
            }
        }
    }
    fn is_maintainer(&mut self, repo: &str, actor: &Actor) -> Result<Option<bool>, ForgeError> {
        let r = self.repo(repo)?;
        self.rt
            .block_on(self.driver.is_maintainer(&r, &actor.login))
            .map(Some)
            .map_err(forgejo_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forge_without_its_credential_has_no_adapter() {
        let rt = runtime().unwrap();
        let cfg = RouterConfig::default();
        assert!(forges(&None, &rt, &cfg, "https://forge.example").is_empty());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FORGEJO_TOKEN), "  \n").unwrap(); // empty = missing
        std::fs::write(dir.path().join(GITHUB_APP_ID), "12").unwrap(); // the other two github files are absent
        assert!(forges(
            &Some(dir.path().to_path_buf()),
            &rt,
            &cfg,
            "https://forge.example"
        )
        .is_empty());
    }

    #[test]
    fn a_present_forgejo_token_gives_an_adapter_that_refuses_a_repository_outside_the_set() {
        let rt = runtime().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FORGEJO_TOKEN), "t\n").unwrap();
        let mut f = forges(
            &Some(dir.path().to_path_buf()),
            &rt,
            &RouterConfig::default(),
            "https://forge.example",
        );
        let adapter = f.get(Forge::Forgejo).expect("forgejo adapter");
        let object = ObjectRef {
            forge: Forge::Forgejo,
            repo: "1".into(),
            kind: cfrg::relay::model::Kind::Issue,
            number: 1,
        };
        // No request is made: the repository id is not in the relay set.
        assert!(matches!(
            adapter.read(&object),
            Err(ForgeError::Rejected(_))
        ));
        assert!(f.get(Forge::Gitlab).is_none() && f.get(Forge::Github).is_none());
    }
}
