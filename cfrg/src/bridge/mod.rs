//! Opt-in, one-shot contribution import. This module never executes imported code.
mod approval;
mod git;
mod http;
mod lifecycle;
mod sandbox;

use crate::{failure, Result};
pub use http::{
    fetch_pages, reject_path_tricks, BridgeAuth, BridgeMethod, BridgeRequest, BridgeTransport,
    MutationPolicy,
};

use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Args)]
/// Read GitLab MRs and explicitly import one into Forgejo; no CI dispatch.
pub struct Options {
    /// Enable this experimental, otherwise inert binary.
    #[arg(long)]
    pub enable_gitlab_import: bool,
    #[arg(long)]
    pub config: PathBuf,
    /// Omit to list open MRs. With this option, default behavior is a read-only plan.
    #[arg(long)]
    pub mr: Option<u64>,
    #[arg(long, requires = "mr")]
    pub apply: bool,
    /// Required for writes: instance hooks and external CI have been audited as disabled.
    #[arg(long, requires = "apply")]
    pub confirm_ci_disabled: bool,
    /// Persistent private state, shared by every invocation for this mapping.
    #[arg(long)]
    pub state_dir: PathBuf,
    /// Reconcile a journaled primary merge/close back to its source MR.
    #[arg(long, requires = "apply", conflicts_with = "replace_head")]
    pub feedback: bool,
    /// Explicit replacement after a rebase; preserves the old branch and PR history.
    #[arg(long, requires = "apply")]
    pub replace_head: Option<String>,
    /// Trusted approval/check request; emit a credential-free isolated Job after live review validation.
    #[arg(long, requires = "mr", conflicts_with = "apply")]
    pub ci_plan: Option<PathBuf>,
    /// Print the exact primary-review text for a trusted CI request, without contacting forges.
    #[arg(long, requires = "ci_plan")]
    pub approval_message: bool,
    /// Submit the approved CI plan after auditing the live isolated namespace.
    #[arg(long, requires_all = ["ci_plan", "kubernetes_command"], conflicts_with = "approval_message")]
    pub dispatch_ci: bool,
    /// Trusted executable wrapper for the deployment's Kubernetes CLI.
    #[arg(long, requires = "dispatch_ci")]
    pub kubernetes_command: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u32,
    /// HTTPS origin, without a path or trailing slash.
    pub gitlab: String,
    pub gitlab_project: u64,
    pub gitlab_repository: String,
    pub forgejo: String,
    /// Primary repository must belong to an organization.
    pub primary_repository: String,
    /// Pre-created direct fork owned by the authenticated bridge account.
    pub import_repository: String,
    pub target_branch: String,
}

impl Config {
    fn validate(&self) -> Result<()> {
        origin(&self.gitlab)?;
        origin(&self.forgejo)?;
        repository(&self.gitlab_repository, false)?;
        repository(&self.primary_repository, true)?;
        repository(&self.import_repository, true)?;
        if self.schema != 1
            || self.gitlab_project == 0
            || self.primary_repository == self.import_repository
            || self.primary_repository.split('/').nth(1) != self.import_repository.split('/').nth(1)
            || !branch(&self.target_branch)
            || self.target_branch.starts_with("ccid-import/")
        {
            return Err(failure("Invalid bridge mapping"));
        }
        Ok(())
    }

    fn key(&self, iid: u64) -> String {
        // Bind state, marker and branch to the complete trusted mapping.
        format!(
            "{:x}",
            Sha256::digest(format!("{}:{iid}", serde_json::to_string(self).unwrap()))
        )
    }

    fn source_url(&self, iid: u64) -> String {
        format!(
            "{}/{}/-/merge_requests/{iid}",
            self.gitlab, self.gitlab_repository
        )
    }
}

fn origin(value: &str) -> Result<()> {
    let host = value.strip_prefix("https://").unwrap_or_default();
    let parts: Vec<_> = host.split(':').collect();
    if parts.len() > 2
        || parts[0].is_empty()
        || parts[0].starts_with('.')
        || !parts[0]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        || (parts.len() == 2 && parts[1].parse::<u16>().ok().filter(|p| *p > 0).is_none())
    {
        return Err(failure(
            "Bridge origins must be credential-free HTTPS origins",
        ));
    }
    Ok(())
}

fn segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn repository(value: &str, pair: bool) -> Result<()> {
    let parts: Vec<_> = value.split('/').collect();
    if value.len() > 512
        || parts.len() < 2
        || (pair && parts.len() != 2)
        || !parts.iter().all(|p| segment(p))
    {
        return Err(failure("Invalid repository path"));
    }
    Ok(())
}

fn branch(value: &str) -> bool {
    value.len() <= 200
        && value.split('/').all(|p| {
            segment(p) && !p.starts_with('.') && !p.ends_with('.') && !p.ends_with(".lock")
        })
        && !value.contains("..")
}

fn oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Author {
    pub id: u64,
    pub username: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeRequest {
    pub iid: u64,
    pub project_id: u64,
    pub target_project_id: u64,
    pub state: String,
    pub title: String,
    pub sha: String,
    pub target_branch: String,
    pub author: Author,
}

impl MergeRequest {
    fn validate(&self, config: &Config, iid: u64) -> Result<()> {
        if iid == 0
            || self.iid != iid
            || self.project_id != config.gitlab_project
            || self.target_project_id != config.gitlab_project
            || !oid(&self.sha)
            || self.target_branch != config.target_branch
            || self.author.id == 0
            || !segment(&self.author.username)
            || self.author.username.len() > 255
            || self.title.is_empty()
            || self.title.len() > 1024
            || self.title.chars().any(char::is_control)
        {
            return Err(failure(
                "MR identity, head, author or target does not match the mapping",
            ));
        }
        if self.state != "opened" {
            return Err(failure("Only open GitLab merge requests can be imported"));
        }
        Ok(())
    }
}

/// The GitLab side of the bridge: merge-request reads plus the two feedback
/// writes (note post, MR close). Implemented once in `cglb`; the orchestrator
/// below only names these typed operations, never GitLab paths or payloads.
/// Mutations are exactly `post_note` and `close_merge_request` — there is no
/// generic mutation path.
pub trait GitlabBridge {
    fn mr_project(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        project: u64,
    ) -> Result<GitlabProject>;
    fn merge_request(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        project: u64,
        iid: u64,
    ) -> Result<MergeRequest>;
    fn open_merge_requests(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        project: u64,
    ) -> Result<Vec<MergeRequestRef>>;
    fn merge_request_notes(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        project: u64,
        iid: u64,
    ) -> Result<Vec<GitlabNote>>;
    fn post_note(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        project: u64,
        iid: u64,
        body: &str,
    ) -> Result<GitlabNote>;
    fn close_merge_request(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        project: u64,
        iid: u64,
    ) -> Result<()>;
    fn current_user_id(&self, transport: &mut dyn BridgeTransport, origin: &str) -> Result<u64>;
    /// Pure constructor for the Git fetch endpoint: the trusted target
    /// project URL plus the merge-request head ref. No I/O.
    fn git_source(&self, origin: &str, repository: &str, iid: u64) -> GitSource;
    /// Authorization scope for exactly this deployment: the validated
    /// origin and project the orchestrator was configured with. The
    /// transport enforces it before any credential lookup.
    fn policy(&self, origin: &str, project: u64) -> Box<dyn MutationPolicy>;
}

/// The Forgejo side of the bridge: repository, pull and review reads plus
/// the two import writes (pull create, predecessor close). Implemented once
/// in `cfgj`. Mutations are exactly `create_pull` and `close_pull`.
pub trait ForgejoBridge {
    fn repository(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        path: &str,
    ) -> Result<ForgejoRepo>;
    fn current_user_id(&self, transport: &mut dyn BridgeTransport, origin: &str) -> Result<u64>;
    fn webhooks_empty(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        path: &str,
    ) -> Result<bool>;
    fn pulls(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        repository: &str,
    ) -> Result<Vec<PullRequest>>;
    fn pull(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        repository: &str,
        index: u64,
    ) -> Result<PullRequest>;
    // Eight explicit arguments (adapter, transport, origin, repository,
    // head, base, title, body): kept separate so each trust-relevant input
    // stays visible at every call site instead of hiding in a struct.
    #[allow(clippy::too_many_arguments)]
    fn create_pull(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        repository: &str,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<PullRequest>;
    fn close_pull(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        repository: &str,
        index: u64,
        body: &str,
    ) -> Result<PullRequest>;
    fn pull_review(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        repository: &str,
        index: u64,
        review: u64,
    ) -> Result<PullReview>;
    fn collaborator_permission(
        &self,
        transport: &mut dyn BridgeTransport,
        origin: &str,
        repository: &str,
        user: &str,
    ) -> Result<CollaboratorPermission>;
    /// Pure constructor for the Git push endpoint: the pre-created import
    /// fork URL plus its owner as the push username. No I/O.
    fn git_dest(&self, origin: &str, repository: &str) -> GitDest;
    /// Authorization scope for exactly this deployment: the validated
    /// origin and primary repository. Enforced before any credential lookup.
    fn policy(&self, origin: &str, primary: &str) -> Box<dyn MutationPolicy>;
}

/// Validated GitLab project identity: numeric id plus exact repository path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitlabProject {
    pub id: u64,
    pub path_with_namespace: String,
    pub visibility: String,
}

/// One open merge request reference from the source inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeRequestRef {
    pub iid: u64,
}

/// One GitLab note: id, exact body and author id. Body and author are
/// optional so malformed notes filter out of matching exactly like the
/// untyped reads did; only matched notes must carry a positive id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitlabNote {
    pub id: Option<u64>,
    pub body: Option<String>,
    pub author_id: Option<u64>,
}

/// Validated Forgejo repository identity and safety posture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgejoRepo {
    pub id: u64,
    pub full_name: String,
    pub private: bool,
    pub has_actions: bool,
    pub fork: bool,
    pub parent_id: Option<u64>,
    pub owner_id: u64,
}

/// One Forgejo pull request, as the bridge identity checks need it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub state: String,
    pub merged: Option<bool>,
    pub head_ref: String,
    pub head_repo_id: u64,
    pub head_sha: String,
    pub base_ref: String,
    pub base_repo_id: u64,
    pub base_sha: String,
    pub user_id: u64,
    pub body: Option<String>,
}

/// One Forgejo review, as the approval binding needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullReview {
    pub id: u64,
    pub user_id: u64,
    pub login: String,
    pub state: String,
    pub dismissed: bool,
    pub stale: bool,
    pub commit_id: String,
    pub body: String,
}

/// One Forgejo collaborator permission answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollaboratorPermission {
    pub user_id: u64,
    pub permission: String,
}

/// Git fetch endpoint for one merge-request head. `token_env` is `None`
/// only for local test transports, never for live forges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSource {
    pub url: String,
    pub token_env: Option<&'static str>,
    pub username: String,
    pub mr_ref: String,
}

/// Git push endpoint for one pre-created import fork.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDest {
    pub url: String,
    pub token_env: Option<&'static str>,
    pub username: String,
}

trait Objects {
    /// Copy exact commits only, creating or advancing the one import ref by fast-forward.
    fn publish(
        &mut self,
        source: &GitSource,
        dest: &GitDest,
        sha: &str,
        branch: &str,
    ) -> Result<()>;
}

/// Fetch one merge request through the adapter, then validate it against the
/// trusted mapping. Identity, head, author and target must all match.
fn mr(
    gitlab: &dyn GitlabBridge,
    transport: &mut dyn BridgeTransport,
    config: &Config,
    iid: u64,
) -> Result<MergeRequest> {
    let result = gitlab.merge_request(transport, &config.gitlab, config.gitlab_project, iid)?;
    result.validate(config, iid)?;
    Ok(result)
}

/// A locked journal survives a crash between mutation intent and response.
struct Journal {
    root: PathBuf,
    _lock: File,
}

impl Journal {
    fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("lock"))?;
        lock.try_lock()
            .map_err(|_| failure("Bridge state is busy; no work started"))?;
        Ok(Self {
            root: root.to_owned(),
            _lock: lock,
        })
    }

    fn load(&self, key: &str) -> Result<Entry> {
        match fs::read(self.root.join(format!("{key}.json"))) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Entry::default()),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, key: &str, value: &impl Serialize) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(self.root.join(format!("{key}.json")))?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    fn admit(&self) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let path = self.root.join("next-poll.json");
        if path.exists() && serde_json::from_slice::<u64>(&fs::read(path)?)? > now {
            return Err(failure("Bridge polling cooldown is active; retry later"));
        }
        self.save("next-poll", &(now + 60))
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    create_attempted: bool,
    pull_number: Option<u64>,
    sha: Option<String>,
    #[serde(default)]
    generation: Option<String>,
    #[serde(default)]
    superseded_pull: Option<u64>,
    #[serde(default)]
    feedback_attempted: bool,
    #[serde(default)]
    feedback_note: Option<u64>,
}

struct Destination {
    primary_id: u64,
    fork_id: u64,
    actor_id: u64,
}

fn preflight(
    gitlab: &dyn GitlabBridge,
    forgejo: &dyn ForgejoBridge,
    transport: &mut dyn BridgeTransport,
    config: &Config,
) -> Result<Destination> {
    let source = gitlab.mr_project(transport, &config.gitlab, config.gitlab_project)?;
    if source.id != config.gitlab_project || source.path_with_namespace != config.gitlab_repository
    {
        return Err(failure("GitLab project ID and repository path disagree"));
    }
    let primary = forgejo.repository(transport, &config.forgejo, &config.primary_repository)?;
    let fork = forgejo.repository(transport, &config.forgejo, &config.import_repository)?;
    let actor_id = forgejo.current_user_id(transport, &config.forgejo)?;
    let primary_id = primary.id;
    if !matches!(
        source.visibility.as_str(),
        "public" | "internal" | "private"
    ) || (source.visibility != "public" && (!primary.private || !fork.private))
        || (primary.private && !fork.private)
    {
        return Err(failure(
            "Import would weaken repository visibility or visibility is unknown",
        ));
    }
    if primary.full_name != config.primary_repository
        || fork.full_name != config.import_repository
        || primary.has_actions
        || fork.has_actions
        || !fork.fork
        || fork.parent_id != Some(primary_id)
        || fork.owner_id != actor_id
    {
        return Err(failure(
            "Import requires a bot-owned direct fork and Actions disabled on both repositories",
        ));
    }
    let owner = config.primary_repository.split('/').next().unwrap();
    for path in [
        format!("repos/{}/hooks", config.primary_repository),
        format!("repos/{}/hooks", config.import_repository),
        format!("orgs/{owner}/hooks"),
        "user/hooks".into(),
    ] {
        if !forgejo.webhooks_empty(transport, &config.forgejo, &path)? {
            return Err(failure(
                "Import requires repository, organization and bridge-user webhooks to be absent",
            ));
        }
    }
    Ok(Destination {
        primary_id,
        fork_id: fork.id,
        actor_id,
    })
}

fn match_pull<'a>(
    pulls: &'a [PullRequest],
    marker: &str,
    branch: &str,
    config: &Config,
    dest: &Destination,
) -> Result<Option<&'a PullRequest>> {
    let mut found = None;
    for pull in pulls {
        let marked = pull
            .body
            .as_ref()
            .is_some_and(|body| body.starts_with(marker));
        let same_head = pull.head_ref == branch && pull.head_repo_id == dest.fork_id;
        if !marked && !same_head {
            continue;
        }
        if !marked
            || !same_head
            || pull.user_id != dest.actor_id
            || pull.base_repo_id != dest.primary_id
            || pull.base_ref != config.target_branch
            || found.is_some()
        {
            return Err(failure(
                "Ambiguous or altered bridge PR identity; manual reconciliation required",
            ));
        }
        found = Some(pull);
    }
    Ok(found)
}

fn import(
    gitlab: &dyn GitlabBridge,
    forgejo: &dyn ForgejoBridge,
    transport: &mut dyn BridgeTransport,
    objects: &mut impl Objects,
    config: &Config,
    iid: u64,
    journal: &Journal,
) -> Result<Value> {
    let current = journal.load(&config.key(iid))?;
    import_generation(
        gitlab,
        forgejo,
        transport,
        objects,
        config,
        iid,
        journal,
        current.generation.as_deref(),
    )
}

fn generation_key(config: &Config, iid: u64, generation: Option<&str>) -> String {
    let key = config.key(iid);
    generation.map_or(key.clone(), |sha| format!("{key}-{sha}"))
}

// Eight explicit dependencies (both adapters, transport, objects, config,
// identity, journal, generation): kept separate for auditability instead
// of a context object that would hide data flow across the trust boundary.
#[allow(clippy::too_many_arguments)]
fn import_generation(
    gitlab: &dyn GitlabBridge,
    forgejo: &dyn ForgejoBridge,
    transport: &mut dyn BridgeTransport,
    objects: &mut impl Objects,
    config: &Config,
    iid: u64,
    journal: &Journal,
    generation: Option<&str>,
) -> Result<Value> {
    let source = mr(gitlab, transport, config, iid)?;
    if generation.is_some_and(|sha| sha != source.sha) {
        return Err(failure(
            "Replacement head changed; explicitly select the new head",
        ));
    }
    let key = generation_key(config, iid, generation);
    let branch = format!("ccid-import/gitlab/{key}");
    let marker = format!("<!-- ccid-pr-bridge:v1:{key} -->\n");
    let mut entry = journal.load(&key)?;
    let dest = preflight(gitlab, forgejo, transport, config)?;
    let pulls = forgejo.pulls(transport, &config.forgejo, &config.primary_repository)?;
    let existing = match_pull(&pulls, &marker, &branch, config, &dest)?;
    if let Some(pull) = existing {
        let index = pull.number;
        if entry.pull_number.is_some_and(|n| n != index) {
            return Err(failure("Stored PR and remote PR disagree"));
        }
        if pull.state == "closed" || pull.merged == Some(true) {
            entry.pull_number = Some(index);
            journal.save(&key, &entry)?;
            return Ok(
                json!({"status":"primary_closed", "pull_number":index, "source":config.source_url(iid), "source_close_pending":true}),
            );
        }
        if pull.state != "open" {
            return Err(failure("Unknown primary PR state"));
        }
    } else if entry.create_attempted || entry.pull_number.is_some() {
        return Err(failure(
            "Unresolved PR creation intent or missing PR; refusing a second POST",
        ));
    }
    // Fetch through the trusted target project MR ref, never a fork URL from JSON.
    objects.publish(
        &gitlab.git_source(&config.gitlab, &config.gitlab_repository, iid),
        &forgejo.git_dest(&config.forgejo, &config.import_repository),
        &source.sha,
        &branch,
    )?;
    // Do not create a PR from a source that closed, retargeted or changed mid-import.
    if mr(gitlab, transport, config, iid)? != source {
        return Err(failure(
            "MR changed during import; reconcile on the next poll",
        ));
    }
    let index = if let Some(pull) = existing {
        pull.number
    } else {
        let owner = config.import_repository.split('/').next().unwrap();
        let title = format!(
            "[WIP] GitLab !{iid}: {}",
            source.title.chars().take(180).collect::<String>()
        );
        let body = format!("{marker}Imported from {}\n\nOriginal author: GitLab `{}` (user ID {}). Original Git commits and authorship are preserved.\n\nUntrusted contribution. CI requires maintainer approval of the current PR head and an isolated runner. CI has not been dispatched by this bridge. Initial imported head: `{}`.\n\nReview the original discussion at the source link; this bridge does not copy reviews or approvals.", config.source_url(iid), source.author.username, source.author.id, source.sha);
        entry.create_attempted = true;
        entry.sha = Some(source.sha.clone());
        journal.save(&key, &entry)?;
        let created = forgejo.create_pull(
            transport,
            &config.forgejo,
            &config.primary_repository,
            &format!("{owner}:{branch}"),
            &config.target_branch,
            &title,
            &body,
        )?;
        let created = [created];
        let pull = match_pull(&created, &marker, &branch, config, &dest)?
            .ok_or_else(|| failure("Created PR identity was not confirmed"))?;
        pull.number
    };
    entry.pull_number = Some(index);
    entry.sha = Some(source.sha.clone());
    journal.save(&key, &entry)?;
    Ok(
        json!({"status":"imported", "source":config.source_url(iid), "head":source.sha, "pull_number":index,
        "pull_url":format!("{}/{}/pulls/{index}", config.forgejo, config.primary_repository), "ci":"not_dispatched"}),
    )
}

pub fn run(
    options: Options,
    gitlab: &dyn GitlabBridge,
    forgejo: &dyn ForgejoBridge,
) -> Result<Value> {
    if !options.enable_gitlab_import {
        return Err(failure(
            "Contribution bridge is disabled; pass --enable-gitlab-import",
        ));
    }
    if options.apply && (!options.confirm_ci_disabled || options.mr.is_none()) {
        return Err(failure("Import requires an MR and --confirm-ci-disabled"));
    }
    let config: Config = toml::from_str(&fs::read_to_string(options.config)?)?;
    config.validate()?;
    if options.approval_message {
        return Ok(
            json!({"review_body":approval::message(&fs::read(options.ci_plan.as_ref().unwrap())?)?}),
        );
    }
    let journal = Journal::open(&options.state_dir)?;
    journal.admit()?;
    let mut transport = http::HttpTransport::new(
        Duration::from_secs(600),
        vec![
            gitlab.policy(&config.gitlab, config.gitlab_project),
            forgejo.policy(&config.forgejo, &config.primary_repository),
        ],
    )?;
    let transport = &mut transport;
    if let Some(iid) = options.mr {
        if let Some(path) = options.ci_plan {
            let bytes = fs::read(path)?;
            let plan = approval::plan(gitlab, forgejo, transport, &config, iid, &journal, &bytes)?;
            if options.dispatch_ci {
                let mut kubernetes = sandbox::Client::new(options.kubernetes_command.unwrap())?;
                sandbox::audit(&mut kubernetes, &plan)?;
                // Re-read review, permissions and contribution immediately before submission.
                let current =
                    approval::plan(gitlab, forgejo, transport, &config, iid, &journal, &bytes)?;
                if current != plan {
                    return Err(failure("Approved CI plan changed before dispatch"));
                }
                let submitted = sandbox::dispatch(&mut kubernetes, &journal, &plan)?;
                return sandbox::wait(&mut kubernetes, &plan, submitted);
            }
            return Ok(plan);
        }
        if options.apply {
            if options.feedback {
                return lifecycle::feedback(gitlab, forgejo, transport, &config, iid, &journal);
            }
            let mut objects = git::Git::new(Duration::from_secs(300))?;
            if let Some(head) = options.replace_head {
                lifecycle::replace(
                    gitlab,
                    forgejo,
                    transport,
                    &mut objects,
                    &config,
                    iid,
                    &journal,
                    &head,
                )
            } else {
                import(
                    gitlab,
                    forgejo,
                    transport,
                    &mut objects,
                    &config,
                    iid,
                    &journal,
                )
            }
        } else {
            let source = mr(gitlab, transport, &config, iid)?;
            Ok(
                json!({"status":"planned", "source":config.source_url(iid), "head":source.sha, "author":source.author.username, "target":config.primary_repository, "ci":"not_dispatched"}),
            )
        }
    } else {
        let refs = gitlab.open_merge_requests(transport, &config.gitlab, config.gitlab_project)?;
        let mut entries = Vec::new();
        for item in refs {
            if entries.iter().any(|v: &Value| v["iid"] == item.iid) {
                return Err(failure("Duplicate MR in paginated inventory"));
            }
            entries.push(json!({"iid":item.iid,"source":config.source_url(item.iid)}));
        }
        Ok(json!({"status":"listed", "merge_requests":entries}))
    }
}

#[cfg(test)]
mod tests;
