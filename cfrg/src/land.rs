//! Landing: only an exact green commit reaches the default branch, one queue
//! per repository, and the agent that requests a landing pushes once and is
//! done.
//!
//! The forge-neutral procedure lives here; adapters implement [`LandTarget`]
//! with the most native mechanism their forge has and declare how native that
//! is ([`Capability`]). Everything the forge keeps (the open pull request is
//! the queue entry, its head commit and status are the evidence) is read back
//! from the forge, so any host can continue another host's queue. The only
//! local state is a small [`Journal`] that stops one-shot side effects
//! (scheduling, retest requests) from repeating.
//!
//! Contract, enforced by [`step`]:
//! * only the head-of-line entry can land, and only when the forge reports a
//!   green verdict for exactly its head commit and the head contains the
//!   default-branch tip (the merge is fast-forward-only and names that head);
//! * when the base moved, the entry is rebased by the forge (never by guesswork
//!   here), native merge scheduling is renewed for the new head, and a retest
//!   is requested; a rebase conflict removes the entry with an explanation;
//! * a failed head never blocks the queue: the next entry becomes head-of-line.
use crate::{failure, model::Forge, native::Endpoint, status::State, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

/// Marks a pull request as a landing queue entry owned by this procedure.
pub const MARKER: &str = "<!-- cfrg-land v1 -->";

/// How much of a procedure the forge provides natively.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Support {
    /// The forge does the whole procedure itself.
    Native,
    /// The forge does the core and cfrg fills a documented gap.
    NativeFill,
    /// No native mechanism; the adapter implements it with plain Git/API calls.
    AdapterOnly,
    /// Not implemented for this forge. Never approximated.
    Unsupported,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Capability {
    pub support: Support,
    pub note: &'static str,
}

impl Capability {
    pub const fn unsupported(note: &'static str) -> Self {
        Self {
            support: Support::Unsupported,
            note,
        }
    }
}

/// One queued landing as the forge reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub number: u64,
    pub branch: String,
    /// Exact commit the landing is about.
    pub head: String,
    /// Default-branch tip at the time of the read.
    pub base_tip: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Merge {
    Merged,
    /// The forge refused for now (mergeability still being computed, checks
    /// not yet accepted); try again on the next pass.
    NotReady,
    /// The branch head is no longer the exact commit that was verified.
    HeadMoved,
}

/// The forge side of landing. Implemented once per adapter.
pub trait LandTarget {
    /// Open queue entries for the default branch, oldest first.
    fn queue(&mut self) -> Result<Vec<Entry>>;
    /// Find or create the queue entry for `branch`.
    fn enqueue(&mut self, branch: &str) -> Result<Entry>;
    /// Whether the entry head already contains the default-branch tip.
    fn contains_tip(&mut self, entry: &Entry) -> Result<bool>;
    /// Latest commit status per context for one exact commit.
    fn statuses(&mut self, sha: &str) -> Result<Vec<(String, State)>>;
    /// Native "merge when checks succeed" with fast-forward-only for the exact
    /// head. It fires on the next success status only; it is not a promise.
    fn schedule(&mut self, entry: &Entry) -> Result<()>;
    /// Whether the forge itself refuses to merge a pull request into the
    /// default branch unless every one of these status contexts succeeded.
    /// Only then is the native merge scheduling safe: on a branch without such
    /// protection the forge treats "no required checks" as success and merges
    /// a head that has no status at all.
    fn gated(&mut self, contexts: &[String]) -> Result<bool>;
    /// Cancel a natively scheduled merge of the entry (none scheduled is fine).
    fn unschedule(&mut self, entry: &Entry) -> Result<()>;
    /// Native rebase of the entry branch onto the default branch. `None`
    /// means the rebase conflicts and nothing changed.
    fn rebase(&mut self, entry: &Entry) -> Result<Option<Entry>>;
    /// The first of `forbidden` that is `head` itself or one of its ancestors,
    /// if any. A forbidden commit the forge does not have cannot be in the
    /// history of any head it holds, so that answers `None`.
    fn forbidden_ancestor(&mut self, head: &str, forbidden: &[String]) -> Result<Option<String>>;
    /// Fast-forward-only merge of exactly `entry.head`.
    fn merge(&mut self, entry: &Entry) -> Result<Merge>;
    /// Remove the entry from the queue, with an explanation for the author.
    fn abandon(&mut self, entry: &Entry, reason: &str) -> Result<()>;
    /// The commit that landed for this entry, when the forge merged it (for
    /// example through native merge scheduling) without cfrg's help.
    fn merged(&mut self, number: u64) -> Result<Option<String>>;
}

/// Commands the procedure may start, fire and forget. `false` means nothing
/// is declared.
pub trait Hooks {
    /// Ask the CI side for a run of an exact commit; the verdict arrives later
    /// as a commit status. Nothing is declared when CI starts from the forge's
    /// own events.
    fn retest(&mut self, repository: &str, entry: &Entry) -> Result<bool>;
    /// An exact commit just reached the default branch (for example: publish
    /// its release).
    fn landed(&mut self, repository: &str, entry: &Entry) -> Result<bool>;
    /// Seconds since the Unix epoch; a hook so tests can move time.
    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// One-shot side effects already done, so a pass is safe to repeat.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    /// Scheduled entries by key, with their branch.
    #[serde(default)]
    pub scheduled: BTreeMap<String, String>,
    #[serde(default)]
    pub retests: BTreeMap<String, u32>,
    /// Pull requests (`repo#number`) whose native merge scheduling has been
    /// settled: cancelled where the branch is not status-gated.
    #[serde(default)]
    pub settled: BTreeSet<String>,
    /// Exact heads (`repo#number@head`) first seen lacking a required
    /// context, with the time. Starts the gate timeout.
    #[serde(default)]
    pub missing_since: BTreeMap<String, u64>,
    /// Exact heads reported blocked and moved out of the queue.
    #[serde(default)]
    pub blocked: BTreeSet<String>,
}

/// A retest is requested at most this often for the same exact commit.
const RETEST_ATTEMPTS: u32 = 2;

impl Journal {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut file, self)?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    }

    fn key(repository: &str, entry: &Entry) -> String {
        format!("{repository}#{}@{}", entry.number, entry.head)
    }

    /// Forget entries that left the queue and return them (head as last
    /// scheduled), so the caller can find out whether they landed.
    fn departed(&mut self, repository: &str, queue: &[Entry]) -> Vec<Entry> {
        let live: BTreeSet<String> = queue.iter().map(|e| Self::key(repository, e)).collect();
        let prefix = format!("{repository}#");
        let mut gone = Vec::new();
        self.scheduled.retain(|key, branch| {
            if !key.starts_with(&prefix) || live.contains(key) {
                return true;
            }
            if let Some((number, head)) = key[prefix.len()..].split_once('@') {
                if let Ok(number) = number.parse() {
                    gone.push(Entry {
                        number,
                        branch: branch.clone(),
                        head: head.into(),
                        base_tip: String::new(),
                    });
                }
            }
            false
        });
        self.retests
            .retain(|k, _| !k.starts_with(&prefix) || live.contains(k));
        self.missing_since
            .retain(|k, _| !k.starts_with(&prefix) || live.contains(k));
        self.blocked
            .retain(|k| !k.starts_with(&prefix) || live.contains(k));
        let numbers: BTreeSet<String> = queue
            .iter()
            .map(|e| format!("{prefix}{}", e.number))
            .collect();
        self.settled
            .retain(|k| !k.starts_with(&prefix) || numbers.contains(k));
        gone
    }

    /// True the first time a pull request is seen.
    fn settle(&mut self, repository: &str, entry: &Entry) -> bool {
        self.settled
            .insert(format!("{repository}#{}", entry.number))
    }

    fn schedule(&mut self, repository: &str, entry: &Entry) -> bool {
        self.scheduled
            .insert(Self::key(repository, entry), entry.branch.clone())
            .is_none()
    }

    fn unschedule(&mut self, repository: &str, entry: &Entry) {
        self.scheduled.remove(&Self::key(repository, entry));
    }
}

/// What to require of a commit and how to ask for a retest.
#[derive(Clone, Debug)]
pub struct Settings {
    pub contexts: Vec<String>,
    /// A head lacking a required context this long after it was first seen is
    /// reported blocked and moved out of the queue.
    pub gate_timeout_seconds: u64,
    /// Commits no landed head may contain (see [`Policy::forbidden_ancestors`]).
    /// Empty: the guard does nothing.
    pub forbidden_ancestors: Vec<String>,
    /// The declared list could not be read: nothing lands until it can.
    pub guard_error: Option<String>,
}

/// Reason given to the author of a head that carries a forbidden commit.
pub const PRE_REWRITE: &str =
    "based on pre-rewrite history; translate the branch with the published map";

/// `*` matches any run of characters; everything else is literal.
pub fn matches(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, rest_parts) = match parts.split_first() {
        Some(split) => split,
        None => return false,
    };
    if rest_parts.is_empty() {
        return pattern == text;
    }
    let Some(mut rest) = text.strip_prefix(*first) else {
        return false;
    };
    let (last, middle) = match rest_parts.split_last() {
        Some(split) => split,
        None => return false,
    };
    for part in middle {
        match rest.find(*part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(*last)
}

/// Required context patterns no status matches.
pub fn missing(statuses: &[(String, State)], contexts: &[String]) -> Vec<String> {
    contexts
        .iter()
        .filter(|p| !statuses.iter().any(|(c, _)| matches(p, c)))
        .cloned()
        .collect()
}

/// One verdict for a commit: every required context pattern must match at
/// least one status, any matching failure fails, anything missing or
/// unfinished is pending.
pub fn verdict(statuses: &[(String, State)], contexts: &[String]) -> State {
    let mut pending = false;
    for pattern in contexts {
        let mut seen = false;
        for (_, state) in statuses.iter().filter(|(c, _)| matches(pattern, c)) {
            seen = true;
            match state {
                State::Failure => return State::Failure,
                State::Pending => pending = true,
                State::Success => {}
            }
        }
        pending |= !seen;
    }
    if pending {
        State::Pending
    } else {
        State::Success
    }
}

/// Head-of-line entry, its verdict and the statuses it was computed from.
type HeadOfLine = (Entry, State, Vec<(String, State)>);

pub struct Report {
    pub events: Vec<Value>,
    /// Something is still queued or waiting for checks; run another pass later.
    pub waiting: bool,
}

/// One idempotent pass over the repository queue. Bounded: each landing
/// re-reads the queue, and at most `ROUNDS` landings or removals happen.
pub fn step(
    target: &mut dyn LandTarget,
    hooks: &mut dyn Hooks,
    settings: &Settings,
    journal: &mut Journal,
    repository: &str,
) -> Result<Report> {
    const ROUNDS: usize = 16;
    let mut events = Vec::new();
    let mut gate = None;
    for _ in 0..ROUNDS {
        let queue = target.queue()?;
        for gone in journal.departed(repository, &queue) {
            if let Some(head) = target.merged(gone.number)? {
                announce_landed(
                    hooks,
                    repository,
                    &Entry { head, ..gone },
                    "forge",
                    &mut events,
                );
            }
        }
        // A merge the forge scheduled on its own is only allowed where the
        // forge also enforces the gate; everywhere else cancel it, once per
        // pull request, so that cfrg alone decides after it saw the status.
        for entry in &queue {
            if journal.settle(repository, entry) && !is_gated(target, settings, &mut gate)? {
                target.unschedule(entry)?;
                events.push(json!({"event":"native-merge-cancelled","number":entry.number}));
            }
        }
        let mut line: Option<HeadOfLine> = None;
        for entry in &queue {
            if line.is_some() {
                events.push(json!({"event":"queued","number":entry.number,"branch":entry.branch}));
                continue;
            }
            if journal.blocked.contains(&Journal::key(repository, entry)) {
                events.push(json!({"event":"blocked","number":entry.number,"branch":entry.branch,"head":entry.head}));
                continue;
            }
            if refuse_forbidden(
                target,
                settings,
                journal,
                repository,
                entry,
                "queue",
                &mut events,
            )? {
                continue;
            }
            let statuses = target.statuses(&entry.head)?;
            let state = verdict(&statuses, &settings.contexts);
            if state == State::Failure {
                events.push(json!({"event":"failed","number":entry.number,"branch":entry.branch,"head":entry.head}));
                continue;
            }
            line = Some((entry.clone(), state, statuses));
        }
        let Some((entry, state, statuses_of_line)) = line else {
            return Ok(Report {
                events,
                waiting: false,
            });
        };
        if !target.contains_tip(&entry)? {
            match target.rebase(&entry)? {
                None => {
                    target.abandon(
                        &entry,
                        "cfrg land: the rebase onto the default branch conflicts. Rebase the branch yourself, push it and land it again.",
                    )?;
                    events.push(
                        json!({"event":"conflict","number":entry.number,"branch":entry.branch}),
                    );
                    continue;
                }
                Some(fresh) => {
                    events.push(json!({"event":"rebased","number":fresh.number,"branch":fresh.branch,"from":entry.head,"to":fresh.head}));
                    journal.unschedule(repository, &entry);
                    arm(target, settings, journal, &mut gate, repository, &fresh)?;
                    request_retest(
                        hooks,
                        journal,
                        repository,
                        &fresh,
                        RETEST_ATTEMPTS,
                        &mut events,
                    )?;
                    return Ok(Report {
                        events,
                        waiting: true,
                    });
                }
            }
        }
        if state == State::Success {
            if refuse_forbidden(
                target,
                settings,
                journal,
                repository,
                &entry,
                "merge",
                &mut events,
            )? {
                continue;
            }
            match target.merge(&entry)? {
                Merge::Merged => {
                    journal.unschedule(repository, &entry);
                    announce_landed(hooks, repository, &entry, "cfrg", &mut events);
                    continue;
                }
                Merge::NotReady => {
                    events.push(json!({"event":"waiting","number":entry.number,"head":entry.head,"reason":"merge-not-ready"}));
                }
                Merge::HeadMoved => {
                    events.push(json!({"event":"waiting","number":entry.number,"head":entry.head,"reason":"head-moved"}));
                }
            }
            return Ok(Report {
                events,
                waiting: true,
            });
        }
        arm(target, settings, journal, &mut gate, repository, &entry)?;
        let no_statuses = statuses_of_line.is_empty();
        let absent = missing(&statuses_of_line, &settings.contexts);
        if !absent.is_empty() {
            let key = Journal::key(repository, &entry);
            let now = hooks.now();
            let since = *journal.missing_since.entry(key.clone()).or_insert(now);
            if now.saturating_sub(since) >= settings.gate_timeout_seconds {
                let reason = format!(
                    "cfrg land: blocked. The required status context(s) {} never appeared on the exact head {} within {} seconds, although the gating run was requested. The entry is removed from the queue so the next one can land; nothing was merged. Fix the gate (or the policy) and land the branch again.",
                    absent.join(", "),
                    entry.head,
                    settings.gate_timeout_seconds
                );
                journal.blocked.insert(key);
                journal.unschedule(repository, &entry);
                target.unschedule(&entry)?;
                let comment = target.abandon(&entry, &reason);
                events.push(json!({"event":"blocked","number":entry.number,"branch":entry.branch,"head":entry.head,"missing":absent,"reason":"required-context-missing-after-timeout","commented":comment.is_ok()}));
                continue;
            }
            request_retest(
                hooks,
                journal,
                repository,
                &entry,
                if no_statuses { RETEST_ATTEMPTS } else { 1 },
                &mut events,
            )?;
        } else {
            journal
                .missing_since
                .remove(&Journal::key(repository, &entry));
        }
        events.push(json!({"event":"waiting","number":entry.number,"head":entry.head,"reason":"checks-pending"}));
        return Ok(Report {
            events,
            waiting: true,
        });
    }
    Ok(Report {
        events,
        waiting: true,
    })
}

/// Refuse a head whose history contains a forbidden commit: no schedule, no
/// rebase, no merge; the entry is commented, closed and remembered as blocked.
/// Returns whether the entry was refused.
fn refuse_forbidden(
    target: &mut dyn LandTarget,
    settings: &Settings,
    journal: &mut Journal,
    repository: &str,
    entry: &Entry,
    stage: &str,
    events: &mut Vec<Value>,
) -> Result<bool> {
    if let Some(error) = &settings.guard_error {
        return Err(failure(format!("Land guard list unusable: {error}")));
    }
    if settings.forbidden_ancestors.is_empty() {
        return Ok(false);
    }
    let Some(found) = target.forbidden_ancestor(&entry.head, &settings.forbidden_ancestors)? else {
        return Ok(false);
    };
    let reason = format!(
        "cfrg land: refused. The head {} contains the forbidden commit {found}: {PRE_REWRITE}. Nothing was rebased or merged.",
        entry.head
    );
    journal.blocked.insert(Journal::key(repository, entry));
    journal.unschedule(repository, entry);
    let cancelled = target.unschedule(entry);
    let comment = target.abandon(entry, &reason);
    events.push(json!({"event":"blocked","number":entry.number,"branch":entry.branch,"head":entry.head,"forbidden":found,"reason":"forbidden-ancestor","stage":stage,"cancelled":cancelled.is_ok(),"commented":comment.is_ok()}));
    Ok(true)
}

fn is_gated(
    target: &mut dyn LandTarget,
    settings: &Settings,
    cache: &mut Option<bool>,
) -> Result<bool> {
    if let Some(known) = *cache {
        return Ok(known);
    }
    let known = target.gated(&settings.contexts)?;
    *cache = Some(known);
    Ok(known)
}

/// Schedule the forge's own merge once per head, but only where the forge
/// gates the branch on the declared contexts.
fn arm(
    target: &mut dyn LandTarget,
    settings: &Settings,
    journal: &mut Journal,
    gate: &mut Option<bool>,
    repository: &str,
    entry: &Entry,
) -> Result<()> {
    if journal.schedule(repository, entry) && is_gated(target, settings, gate)? {
        target.schedule(entry)?;
    }
    Ok(())
}

fn announce_landed(
    hooks: &mut dyn Hooks,
    repository: &str,
    entry: &Entry,
    by: &str,
    events: &mut Vec<Value>,
) {
    events.push(json!({"event":"landed","number":entry.number,"branch":entry.branch,"head":entry.head,"by":by}));
    match hooks.landed(repository, entry) {
        Ok(true) => events.push(json!({"event":"landed-hook-requested","number":entry.number,"head":entry.head})),
        Ok(false) => {}
        Err(e) => events.push(json!({"event":"landed-hook-failed","number":entry.number,"head":entry.head,"error":e.to_string()})),
    }
}

fn request_retest(
    hooks: &mut dyn Hooks,
    journal: &mut Journal,
    repository: &str,
    entry: &Entry,
    limit: u32,
    events: &mut Vec<Value>,
) -> Result<()> {
    let attempts = journal
        .retests
        .entry(Journal::key(repository, entry))
        .or_insert(0);
    if *attempts >= limit {
        return Ok(());
    }
    *attempts += 1;
    match hooks.retest(repository, entry) {
        Ok(true) => events.push(json!({"event":"retest-requested","number":entry.number,"head":entry.head})),
        Ok(false) => {}
        Err(e) => events.push(json!({"event":"retest-failed","number":entry.number,"head":entry.head,"error":e.to_string()})),
    }
    Ok(())
}

/// Put `branch` in the queue and, where the forge gates the branch, schedule
/// the native merge; nothing more:
/// `cfrg serve` reacts to the pull request event and finishes the landing.
pub fn enqueue(
    target: &mut dyn LandTarget,
    settings: &Settings,
    journal: &mut Journal,
    repository: &str,
    branch: &str,
) -> Result<Report> {
    let entry = target.enqueue(branch)?;
    let mut events = Vec::new();
    if refuse_forbidden(
        target,
        settings,
        journal,
        repository,
        &entry,
        "enqueue",
        &mut events,
    )? {
        events.insert(
            0,
            json!({"event":"enqueued","number":entry.number,"branch":entry.branch,"head":entry.head}),
        );
        return Ok(Report {
            events,
            waiting: false,
        });
    }
    let mut gate = None;
    if journal.settle(repository, &entry) && !is_gated(target, settings, &mut gate)? {
        target.unschedule(&entry)?;
    }
    arm(target, settings, journal, &mut gate, repository, &entry)?;
    Ok(Report {
        events: vec![
            json!({"event":"enqueued","number":entry.number,"branch":entry.branch,"head":entry.head}),
            json!({"event":"handed-to-serve","number":entry.number}),
        ],
        waiting: true,
    })
}

/// Put `branch` in the queue and run one pass, so a branch that is already
/// green and current lands immediately and everything else is scheduled.
pub fn request(
    target: &mut dyn LandTarget,
    hooks: &mut dyn Hooks,
    settings: &Settings,
    journal: &mut Journal,
    repository: &str,
    branch: &str,
) -> Result<Report> {
    let entry = target.enqueue(branch)?;
    let mut report = step(target, hooks, settings, journal, repository)?;
    report.events.insert(
        0,
        json!({"event":"enqueued","number":entry.number,"branch":entry.branch,"head":entry.head}),
    );
    Ok(report)
}

/// Branch names are plain Git ref names without anything a URL, a command
/// line or a path could reinterpret.
pub fn branch_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 200
        || value.starts_with(['-', '/', '.'])
        || value.ends_with(['/', '.'])
        || value.ends_with(".lock")
        || value.contains("..")
        || value.contains("//")
        || value.contains("@{")
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
    {
        return Err(failure("Invalid branch name"));
    }
    Ok(())
}

pub fn commit_id(value: &str) -> Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(failure("Expected a full lowercase commit id"));
    }
    Ok(())
}

/// Who finishes a landing after the request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Follow {
    /// A detached follower process started by `cfrg land` on the same host.
    #[default]
    Detached,
    /// `cfrg serve` owns every queue: the request only enqueues, the forge's
    /// own events and the serve sweep do the rest.
    Serve,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Enforcement {
    /// Merges require the CI status; pushes stay as they are. Julian's
    /// decision on strictness is pending, so nothing stricter exists.
    #[default]
    Soft,
}

/// Declared landing policy: one file in a repository, applied by cfrg.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema: u32,
    pub forge: Forge,
    pub endpoint: Endpoint,
    /// Required status context patterns for every repository.
    #[serde(default = "default_contexts")]
    pub contexts: Vec<String>,
    #[serde(default)]
    pub enforcement: Enforcement,
    #[serde(default)]
    pub follow: Follow,
    /// The long-running mode (`cfrg serve`).
    pub serve: Option<crate::serve::ServePolicy>,
    /// A queued head still lacking a required context this long after it was
    /// first seen is reported blocked and removed from the queue.
    #[serde(default = "default_gate_timeout")]
    pub gate_timeout_seconds: u64,
    /// Seconds between passes of the unattended follower.
    #[serde(default = "default_interval")]
    pub interval_seconds: u64,
    /// The follower gives up after this long without an empty queue.
    #[serde(default = "default_follow")]
    pub follow_seconds: u64,
    /// Command asked for a CI run of an exact commit; placeholders
    /// `{repository}`, `{branch}`, `{sha}`, `{origin}` and `{checkout}` (a
    /// local checkout of exactly that commit, prepared by cfrg). Optional when
    /// CI starts from the forge's own push events.
    #[serde(default)]
    pub retest: Vec<String>,
    /// Command started when an exact commit has reached the default branch
    /// (same placeholders), for example to publish its release.
    #[serde(default)]
    pub landed: Vec<String>,
    /// Commits that no landed head may contain, for every repository: the
    /// old root commits of rewritten histories. A head whose ancestry holds
    /// one is refused (never rebased, never merged).
    #[serde(default)]
    pub forbidden_ancestors: Vec<String>,
    /// Optional JSON file with more forbidden commits, re-read on every pass
    /// so a published old-to-new map can be dropped in without a code change
    /// or a restart: `{"schema":1,"forbidden_ancestors":{"<org>/<repo>":
    /// ["<40 hex>", ...], "*": ["<40 hex>", ...]}}` (`*` applies to every
    /// repository). Unreadable or malformed: nothing lands (fail closed).
    #[serde(default)]
    pub forbidden_ancestors_file: Option<String>,
    #[serde(default)]
    pub repositories: Vec<RepositoryPolicy>,
}

/// Shape of [`Policy::forbidden_ancestors_file`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForbiddenFile {
    schema: u32,
    forbidden_ancestors: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryPolicy {
    pub path: String,
    pub contexts: Option<Vec<String>>,
    pub retest: Option<Vec<String>>,
    pub landed: Option<Vec<String>>,
    /// Overrides the policy-wide gate timeout.
    pub gate_timeout_seconds: Option<u64>,
    /// Forbidden commits for this repository, added to the policy-wide ones.
    #[serde(default)]
    pub forbidden_ancestors: Vec<String>,
}

fn default_contexts() -> Vec<String> {
    vec!["ci/*".into()]
}
fn default_gate_timeout() -> u64 {
    45 * 60
}
fn default_interval() -> u64 {
    30
}
fn default_follow() -> u64 {
    4 * 3600
}

impl Policy {
    pub fn load(path: &Path) -> Result<Self> {
        let policy: Self = serde_json::from_slice(&fs::read(path)?)?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != 1 {
            return Err(failure("Unsupported land policy schema"));
        }
        self.endpoint.validate()?;
        patterns(&self.contexts)?;
        command(&self.retest)?;
        command(&self.landed)?;
        if let Some(serve) = &self.serve {
            serve.validate()?;
        }
        if self.follow == Follow::Serve && self.serve.is_none() {
            return Err(failure("follow = serve needs a serve block"));
        }
        commits(&self.forbidden_ancestors)?;
        if let Some(file) = &self.forbidden_ancestors_file {
            if file.is_empty() || file.contains('\0') {
                return Err(failure("Invalid forbidden ancestors file path"));
            }
        }
        for repo in &self.repositories {
            commits(&repo.forbidden_ancestors)?;
        }
        let timeout_ok = |t: u64| (60..=86_400).contains(&t);
        if !timeout_ok(self.gate_timeout_seconds)
            || self
                .repositories
                .iter()
                .any(|r| r.gate_timeout_seconds.is_some_and(|t| !timeout_ok(t)))
        {
            return Err(failure("Land gate timeout out of range"));
        }
        if !(5..=3600).contains(&self.interval_seconds)
            || !(60..=172_800).contains(&self.follow_seconds)
        {
            return Err(failure("Land follower interval or duration out of range"));
        }
        let mut seen = BTreeSet::new();
        for repo in &self.repositories {
            crate::native::path(&repo.path, false)?;
            if !seen.insert(&repo.path) {
                return Err(failure("Duplicate repository in land policy"));
            }
            if let Some(contexts) = &repo.contexts {
                patterns(contexts)?;
            }
            if let Some(retest) = &repo.retest {
                command(retest)?;
            }
            if let Some(landed) = &repo.landed {
                command(landed)?;
            }
        }
        Ok(())
    }

    fn repository(&self, path: &str) -> Option<&RepositoryPolicy> {
        self.repositories.iter().find(|r| r.path == path)
    }

    pub fn contexts(&self, path: &str) -> Vec<String> {
        self.repository(path)
            .and_then(|r| r.contexts.clone())
            .unwrap_or_else(|| self.contexts.clone())
    }

    pub fn gate_timeout(&self, path: &str) -> u64 {
        self.repository(path)
            .and_then(|r| r.gate_timeout_seconds)
            .unwrap_or(self.gate_timeout_seconds)
    }

    pub fn retest(&self, path: &str) -> Vec<String> {
        self.repository(path)
            .and_then(|r| r.retest.clone())
            .unwrap_or_else(|| self.retest.clone())
    }

    pub fn landed(&self, path: &str) -> Vec<String> {
        self.repository(path)
            .and_then(|r| r.landed.clone())
            .unwrap_or_else(|| self.landed.clone())
    }

    /// Forbidden commits for one repository: policy-wide, per-repository and
    /// the optional file (read now). Deduplicated.
    pub fn forbidden(&self, path: &str) -> Result<Vec<String>> {
        let mut all: BTreeSet<String> = self.forbidden_ancestors.iter().cloned().collect();
        if let Some(repo) = self.repository(path) {
            all.extend(repo.forbidden_ancestors.iter().cloned());
        }
        if let Some(file) = &self.forbidden_ancestors_file {
            let parsed: ForbiddenFile = serde_json::from_slice(&fs::read(file)?)?;
            if parsed.schema != 1 {
                return Err(failure("Unsupported forbidden ancestors schema"));
            }
            for list in parsed.forbidden_ancestors.values() {
                commits(list)?;
            }
            for key in ["*", path] {
                if let Some(list) = parsed.forbidden_ancestors.get(key) {
                    all.extend(list.iter().cloned());
                }
            }
        }
        Ok(all.into_iter().collect())
    }

    pub fn settings(&self, path: &str) -> Settings {
        let (forbidden_ancestors, guard_error) = match self.forbidden(path) {
            Ok(list) => (list, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        Settings {
            contexts: self.contexts(path),
            gate_timeout_seconds: self.gate_timeout(path),
            forbidden_ancestors,
            guard_error,
        }
    }
}

fn commits(values: &[String]) -> Result<()> {
    values.iter().try_for_each(|v| commit_id(v))
}

fn patterns(values: &[String]) -> Result<()> {
    if values.is_empty()
        || values.iter().any(|p| {
            p.is_empty()
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-/:*".contains(&b))
        })
    {
        return Err(failure("Invalid status context pattern"));
    }
    Ok(())
}

fn command(argv: &[String]) -> Result<()> {
    for arg in argv {
        let mut rest = arg.as_str();
        while let Some(open) = rest.find('{') {
            let close = rest[open..]
                .find('}')
                .ok_or_else(|| failure("Unterminated placeholder in retest command"))?;
            let name = &rest[open + 1..open + close];
            if !["repository", "branch", "sha", "origin", "checkout"].contains(&name) {
                return Err(failure("Unknown placeholder in retest command"));
            }
            rest = &rest[open + close + 1..];
        }
        if arg.contains('\0') {
            return Err(failure("Invalid retest command"));
        }
    }
    Ok(())
}

/// Whether a command wants a local checkout of the exact commit.
pub fn wants_checkout(argv: &[String]) -> bool {
    argv.iter().any(|arg| arg.contains("{checkout}"))
}

/// Fill the placeholders of a hook command from validated values.
pub fn expand(
    argv: &[String],
    origin: &str,
    repository: &str,
    entry: &Entry,
    checkout: &str,
) -> Vec<String> {
    argv.iter()
        .map(|arg| {
            arg.replace("{repository}", repository)
                .replace("{branch}", &entry.branch)
                .replace("{sha}", &entry.head)
                .replace("{origin}", origin)
                .replace("{checkout}", checkout)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        queue: Vec<Entry>,
        statuses: BTreeMap<String, Vec<(String, State)>>,
        behind: BTreeSet<u64>,
        conflicts: BTreeSet<u64>,
        /// Entries the forge merged on its own, with the landed commit.
        merged: BTreeMap<u64, String>,
        /// The default branch has no status-gated protection.
        ungated: bool,
        /// The branch head moves between verification and merge.
        moved: bool,
        log: Vec<String>,
        tip: String,
        /// Commits in each head's history, by head.
        history: BTreeMap<String, Vec<String>>,
    }

    fn entry(number: u64, head: &str) -> Entry {
        Entry {
            number,
            branch: format!("b{number}"),
            head: head.into(),
            base_tip: "tip".into(),
        }
    }

    impl LandTarget for Fake {
        fn queue(&mut self) -> Result<Vec<Entry>> {
            Ok(self.queue.clone())
        }
        fn enqueue(&mut self, branch: &str) -> Result<Entry> {
            self.log.push(format!("enqueue {branch}"));
            self.queue
                .iter()
                .find(|e| e.branch == branch)
                .cloned()
                .ok_or_else(|| failure("unknown branch"))
        }
        fn contains_tip(&mut self, entry: &Entry) -> Result<bool> {
            Ok(!self.behind.contains(&entry.number))
        }
        fn statuses(&mut self, sha: &str) -> Result<Vec<(String, State)>> {
            Ok(self.statuses.get(sha).cloned().unwrap_or_default())
        }
        fn schedule(&mut self, entry: &Entry) -> Result<()> {
            self.log
                .push(format!("schedule {}@{}", entry.number, entry.head));
            Ok(())
        }
        fn gated(&mut self, _contexts: &[String]) -> Result<bool> {
            Ok(!self.ungated)
        }
        fn unschedule(&mut self, entry: &Entry) -> Result<()> {
            self.log.push(format!("unschedule {}", entry.number));
            Ok(())
        }
        fn rebase(&mut self, entry: &Entry) -> Result<Option<Entry>> {
            if self.conflicts.contains(&entry.number) {
                return Ok(None);
            }
            self.behind.remove(&entry.number);
            let fresh = Entry {
                head: format!("{}r", entry.head),
                base_tip: self.tip.clone(),
                ..entry.clone()
            };
            for queued in &mut self.queue {
                if queued.number == fresh.number {
                    *queued = fresh.clone();
                }
            }
            self.log.push(format!("rebase {}", entry.number));
            Ok(Some(fresh))
        }
        fn forbidden_ancestor(
            &mut self,
            head: &str,
            forbidden: &[String],
        ) -> Result<Option<String>> {
            self.log.push(format!("ancestry {head}"));
            let history = self.history.get(head).cloned().unwrap_or_default();
            Ok(forbidden.iter().find(|f| history.contains(f)).cloned())
        }
        fn merge(&mut self, entry: &Entry) -> Result<Merge> {
            if self.moved {
                return Ok(Merge::HeadMoved);
            }
            self.log
                .push(format!("merge {}@{}", entry.number, entry.head));
            self.queue.retain(|e| e.number != entry.number);
            // The base moved: every remaining entry is now behind.
            self.behind.extend(self.queue.iter().map(|e| e.number));
            Ok(Merge::Merged)
        }
        fn abandon(&mut self, entry: &Entry, _reason: &str) -> Result<()> {
            self.log.push(format!("abandon {}", entry.number));
            self.queue.retain(|e| e.number != entry.number);
            Ok(())
        }
        fn merged(&mut self, number: u64) -> Result<Option<String>> {
            Ok(self.merged.get(&number).cloned())
        }
    }

    #[derive(Default)]
    struct Hook {
        retests: Vec<String>,
        landed: Vec<String>,
        clock: u64,
    }
    impl Hooks for Hook {
        fn now(&self) -> u64 {
            self.clock
        }
        fn retest(&mut self, _repository: &str, entry: &Entry) -> Result<bool> {
            self.retests.push(entry.head.clone());
            Ok(true)
        }
        fn landed(&mut self, _repository: &str, entry: &Entry) -> Result<bool> {
            self.landed.push(entry.head.clone());
            Ok(true)
        }
    }

    fn green(sha: &str) -> (String, Vec<(String, State)>) {
        (sha.into(), vec![("ci/crow/x".into(), State::Success)])
    }
    fn settings() -> Settings {
        Settings {
            contexts: vec!["ci/*".into()],
            gate_timeout_seconds: 2700,
            forbidden_ancestors: Vec::new(),
            guard_error: None,
        }
    }

    const OLD_ROOT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn guarded() -> Settings {
        Settings {
            forbidden_ancestors: vec![OLD_ROOT.into()],
            ..settings()
        }
    }

    #[test]
    fn head_with_a_forbidden_ancestor_is_refused_before_any_rebase_or_merge() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            behind: BTreeSet::from([1]),
            ..Fake::default()
        };
        fake.statuses.extend([green("h1")]);
        fake.history.insert("h1".into(), vec![OLD_ROOT.into()]);
        let mut journal = Journal::default();
        let report = step(
            &mut fake,
            &mut Hook::default(),
            &guarded(),
            &mut journal,
            "o/r",
        )
        .unwrap();
        assert_eq!(fake.log, ["ancestry h1", "unschedule 1", "abandon 1"]);
        assert!(report.events.iter().any(|e| e["event"] == "blocked"
            && e["reason"] == "forbidden-ancestor"
            && e["forbidden"] == OLD_ROOT));
        assert!(!report.waiting);
    }

    #[test]
    fn refused_enqueue_schedules_nothing() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            ..Fake::default()
        };
        fake.history.insert("h1".into(), vec![OLD_ROOT.into()]);
        enqueue(&mut fake, &guarded(), &mut Journal::default(), "o/r", "b1").unwrap();
        assert!(!fake.log.iter().any(|l| l.starts_with("schedule")));
        assert!(fake.log.contains(&"abandon 1".to_string()));
    }

    #[test]
    fn refusal_does_not_block_the_clean_entry_behind_it() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1"), entry(2, "h2")],
            ..Fake::default()
        };
        fake.statuses.extend([green("h1"), green("h2")]);
        fake.history.insert("h1".into(), vec![OLD_ROOT.into()]);
        step(
            &mut fake,
            &mut Hook::default(),
            &guarded(),
            &mut Journal::default(),
            "o/r",
        )
        .unwrap();
        assert!(fake.log.contains(&"merge 2@h2".to_string()));
        assert!(!fake.log.iter().any(|l| l.starts_with("merge 1")));
    }

    #[test]
    fn clean_head_and_empty_list_are_unaffected() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            ..Fake::default()
        };
        fake.statuses.extend([green("h1")]);
        fake.history.insert("h1".into(), vec!["other".into()]);
        step(
            &mut fake,
            &mut Hook::default(),
            &guarded(),
            &mut Journal::default(),
            "o/r",
        )
        .unwrap();
        assert_eq!(fake.log, ["ancestry h1", "ancestry h1", "merge 1@h1"]);
        // Empty list: no ancestry query at all, even for a poisoned history.
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            ..Fake::default()
        };
        fake.statuses.extend([green("h1")]);
        fake.history.insert("h1".into(), vec![OLD_ROOT.into()]);
        step(
            &mut fake,
            &mut Hook::default(),
            &settings(),
            &mut Journal::default(),
            "o/r",
        )
        .unwrap();
        assert_eq!(fake.log, ["merge 1@h1"]);
    }

    #[test]
    fn an_unreadable_list_fails_closed() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            ..Fake::default()
        };
        fake.statuses.extend([green("h1")]);
        let broken = Settings {
            guard_error: Some("no such file".into()),
            ..settings()
        };
        assert!(step(
            &mut fake,
            &mut Hook::default(),
            &broken,
            &mut Journal::default(),
            "o/r"
        )
        .is_err());
        assert!(fake.log.is_empty());
    }

    #[test]
    fn policy_collects_inline_per_repository_and_file_lists() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let c = "c".repeat(40);
        let dir = std::env::temp_dir().join(format!("cfrg-forbidden-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("map.json");
        fs::write(
            &file,
            format!(r#"{{"schema":1,"forbidden_ancestors":{{"*":["{c}"],"o/r":["{b}"],"o/other":["{a}"]}}}}"#),
        )
        .unwrap();
        let policy: Policy = serde_json::from_value(json!({
            "schema":1,"forge":"forgejo",
            "endpoint":{"origin":"https://forge.example","token_env":"T"},
            "forbidden_ancestors":[a],
            "forbidden_ancestors_file": file.to_str().unwrap(),
            "repositories":[{"path":"o/r","forbidden_ancestors":[b]}]
        }))
        .unwrap();
        policy.validate().unwrap();
        assert_eq!(
            policy.forbidden("o/r").unwrap(),
            [a.clone(), b.clone(), c.clone()]
        );
        assert_eq!(policy.forbidden("o/x").unwrap(), [a.clone(), c.clone()]);
        fs::write(&file, "not json").unwrap();
        assert!(policy.settings("o/r").guard_error.is_some());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn glob_and_verdict() {
        assert!(matches("ci/*", "ci/crow/manual/x"));
        assert!(matches("*", "anything"));
        assert!(matches("a*b*c", "a--b--c"));
        assert!(!matches("a*b", "a"));
        assert!(!matches("ci/*", "other/ci/x"));
        assert!(matches("exact", "exact") && !matches("exact", "exactly"));
        let ok = |s: &str| (s.to_string(), State::Success);
        let ctx = vec!["ci/*".to_string()];
        assert_eq!(verdict(&[], &ctx), State::Pending);
        assert_eq!(verdict(&[ok("ci/a")], &ctx), State::Success);
        assert_eq!(
            verdict(&[ok("ci/a"), ("ci/b".into(), State::Pending)], &ctx),
            State::Pending
        );
        assert_eq!(
            verdict(&[ok("ci/a"), ("ci/b".into(), State::Failure)], &ctx),
            State::Failure
        );
        assert_eq!(
            verdict(&[("other".into(), State::Failure)], &ctx),
            State::Pending
        );
        let two = vec!["ci/*".to_string(), "lint/*".to_string()];
        assert_eq!(verdict(&[ok("ci/a")], &two), State::Pending);
    }

    #[test]
    fn green_and_current_lands_exactly_that_head() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            ..Fake::default()
        };
        fake.statuses.extend([green("h1")]);
        let mut hook = Hook::default();
        let report = step(
            &mut fake,
            &mut hook,
            &settings(),
            &mut Journal::default(),
            "o/r",
        )
        .unwrap();
        assert_eq!(fake.log, ["merge 1@h1"]);
        assert!(!report.waiting);
        assert!(hook.retests.is_empty());
    }

    fn gate_settings() -> Settings {
        Settings {
            contexts: vec!["ci/crow/*".into(), "ci/gate".into()],
            gate_timeout_seconds: 100,
            forbidden_ancestors: Vec::new(),
            guard_error: None,
        }
    }

    #[test]
    fn missing_context_is_submitted_once_per_head() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            ..Fake::default()
        };
        fake.statuses
            .insert("h1".into(), vec![("ci/crow/x".into(), State::Success)]);
        let mut hook = Hook::default();
        let mut journal = Journal::default();
        for t in [0, 10, 20, 30] {
            hook.clock = t;
            let report = step(&mut fake, &mut hook, &gate_settings(), &mut journal, "o/r").unwrap();
            assert!(report.waiting);
        }
        assert_eq!(hook.retests, ["h1"]);
        assert!(!fake.log.iter().any(|l| l.starts_with("merge")));
    }

    #[test]
    fn missing_context_after_timeout_is_reported_and_skipped() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1"), entry(2, "h2")],
            ..Fake::default()
        };
        fake.statuses
            .insert("h1".into(), vec![("ci/crow/x".into(), State::Success)]);
        fake.statuses.insert(
            "h2".into(),
            vec![
                ("ci/crow/x".into(), State::Success),
                ("ci/gate".into(), State::Success),
            ],
        );
        let mut hook = Hook::default();
        let mut journal = Journal::default();
        step(&mut fake, &mut hook, &gate_settings(), &mut journal, "o/r").unwrap();
        assert_eq!(hook.retests, ["h1"]);
        hook.clock = 100;
        let report = step(&mut fake, &mut hook, &gate_settings(), &mut journal, "o/r").unwrap();
        assert!(report
            .events
            .iter()
            .any(|e| e["event"] == "blocked" && e["number"] == 1 && e["missing"][0] == "ci/gate"));
        assert_eq!(
            fake.log,
            ["schedule 1@h1", "unschedule 1", "abandon 1", "merge 2@h2"]
        );
        assert_eq!(hook.retests, ["h1"]);
    }

    #[test]
    fn missing_context_that_turns_green_lands() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            ..Fake::default()
        };
        fake.statuses
            .insert("h1".into(), vec![("ci/crow/x".into(), State::Success)]);
        let mut hook = Hook::default();
        let mut journal = Journal::default();
        step(&mut fake, &mut hook, &gate_settings(), &mut journal, "o/r").unwrap();
        fake.statuses.insert(
            "h1".into(),
            vec![
                ("ci/crow/x".into(), State::Success),
                ("ci/gate".into(), State::Success),
            ],
        );
        hook.clock = 99;
        step(&mut fake, &mut hook, &gate_settings(), &mut journal, "o/r").unwrap();
        assert!(fake.log.contains(&"merge 1@h1".to_string()));
        assert_eq!(hook.retests, ["h1"]);
    }

    #[test]
    fn pending_is_scheduled_once_and_empty_checks_ask_for_a_retest_twice_at_most() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            ..Fake::default()
        };
        let mut hook = Hook::default();
        let mut journal = Journal::default();
        for _ in 0..4 {
            let report = step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
            assert!(report.waiting);
        }
        assert_eq!(fake.log, ["schedule 1@h1"]);
        assert_eq!(hook.retests, ["h1", "h1"]);
    }

    #[test]
    fn behind_head_is_rebased_rescheduled_and_retested_then_lands_after_green() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            behind: BTreeSet::from([1]),
            tip: "tip2".into(),
            ..Fake::default()
        };
        fake.statuses.extend([green("h1")]);
        let mut hook = Hook::default();
        let mut journal = Journal::default();
        let report = step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert!(report.waiting);
        assert_eq!(fake.log, ["rebase 1", "schedule 1@h1r"]);
        assert_eq!(hook.retests, ["h1r"]);
        // The rebased head has no checks yet: still waiting, no landing.
        step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert!(!fake.log.iter().any(|l| l.starts_with("merge")));
        fake.statuses.extend([green("h1r")]);
        let report = step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert_eq!(fake.log.last().unwrap(), "merge 1@h1r");
        assert!(!report.waiting);
    }

    #[test]
    fn one_queue_second_entry_waits_then_is_rebased_after_the_first_lands() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1"), entry(2, "h2")],
            tip: "tip2".into(),
            ..Fake::default()
        };
        fake.statuses.extend([green("h1"), green("h2")]);
        let mut hook = Hook::default();
        let report = step(
            &mut fake,
            &mut hook,
            &settings(),
            &mut Journal::default(),
            "o/r",
        )
        .unwrap();
        // Entry 1 lands; entry 2 was green but its base moved, so it is rebased
        // and must pass again before it can land. Never landed unverified.
        assert_eq!(fake.log, ["merge 1@h1", "rebase 2", "schedule 2@h2r"]);
        assert!(report.waiting);
        assert_eq!(hook.retests, ["h2r"]);
    }

    #[test]
    fn conflict_removes_the_entry_and_the_next_one_proceeds() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1"), entry(2, "h2")],
            behind: BTreeSet::from([1]),
            conflicts: BTreeSet::from([1]),
            ..Fake::default()
        };
        fake.statuses.extend([green("h2")]);
        let report = step(
            &mut fake,
            &mut Hook::default(),
            &settings(),
            &mut Journal::default(),
            "o/r",
        )
        .unwrap();
        assert_eq!(fake.log, ["abandon 1", "merge 2@h2"]);
        assert!(!report.waiting);
    }

    #[test]
    fn failed_head_does_not_block_the_queue() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1"), entry(2, "h2")],
            ..Fake::default()
        };
        fake.statuses
            .insert("h1".into(), vec![("ci/x".into(), State::Failure)]);
        fake.statuses.extend([green("h2")]);
        let report = step(
            &mut fake,
            &mut Hook::default(),
            &settings(),
            &mut Journal::default(),
            "o/r",
        )
        .unwrap();
        assert_eq!(fake.log, ["merge 2@h2"]);
        assert!(report.events.iter().any(|e| e["event"] == "failed"));
        assert!(!report.waiting);
    }

    #[test]
    fn handing_over_only_enqueues_and_schedules_once() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            ..Fake::default()
        };
        let mut journal = Journal::default();
        let report = enqueue(&mut fake, &settings(), &mut journal, "o/r", "b1").unwrap();
        enqueue(&mut fake, &settings(), &mut journal, "o/r", "b1").unwrap();
        assert_eq!(fake.log, ["enqueue b1", "schedule 1@h1", "enqueue b1"]);
        assert!(report.waiting);
        assert_eq!(report.events[1]["event"], "handed-to-serve");
    }

    #[test]
    fn follow_serve_needs_a_serve_block() {
        let base = json!({
            "schema":1,"forge":"forgejo",
            "endpoint":{"origin":"https://forge.example","token_env":"TOKEN"},
            "follow":"serve"
        });
        let policy: Policy = serde_json::from_value(base.clone()).unwrap();
        assert!(policy.validate().is_err());
        let mut with = base;
        with["serve"] = json!({"listen":"127.0.0.1:8080","secret_env":"SECRET"});
        let policy: Policy = serde_json::from_value(with).unwrap();
        policy.validate().unwrap();
        assert_eq!(policy.follow, Follow::Serve);
    }

    #[test]
    fn request_enqueues_then_steps() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            ..Fake::default()
        };
        fake.statuses.extend([green("h1")]);
        let report = request(
            &mut fake,
            &mut Hook::default(),
            &settings(),
            &mut Journal::default(),
            "o/r",
            "b1",
        )
        .unwrap();
        assert_eq!(fake.log, ["enqueue b1", "merge 1@h1"]);
        assert_eq!(report.events[0]["event"], "enqueued");
    }

    #[test]
    fn validators_reject_unsafe_values() {
        for bad in [
            "", "-x", "/x", "a..b", "a b", "a;b", "x.lock", "a//b", "a@{b", "x/",
        ] {
            assert!(branch_name(bad).is_err(), "{bad}");
        }
        assert!(branch_name("ci/land-1.x").is_ok());
        assert!(commit_id(&"a".repeat(40)).is_ok());
        assert!(commit_id("ABC").is_err());
        assert!(command(&["x".into(), "{sha}".into()]).is_ok());
        assert!(command(&["{unknown}".into()]).is_err());
    }

    #[test]
    fn policy_defaults_and_overrides() {
        let policy: Policy = serde_json::from_value(json!({
            "schema":1,"forge":"forgejo",
            "endpoint":{"origin":"https://forge.example","token_env":"TOKEN"},
            "landed":["publish","{sha}"],
            "repositories":[{"path":"o/r","contexts":["ci/crow/*"],"retest":["run","{checkout}","{sha}"]}]
        }))
        .unwrap();
        policy.validate().unwrap();
        assert_eq!(policy.contexts("o/r"), ["ci/crow/*"]);
        assert_eq!(policy.contexts("o/other"), ["ci/*"]);
        let entry = entry(1, &"a".repeat(40));
        assert_eq!(
            expand(
                &policy.retest("o/r"),
                "https://forge.example",
                "o/r",
                &entry,
                "/work/o-r"
            ),
            ["run".to_string(), "/work/o-r".to_string(), "a".repeat(40)]
        );
        assert!(wants_checkout(&policy.retest("o/r")));
        assert!(!wants_checkout(&policy.landed("o/r")));
        assert!(policy.retest("o/other").is_empty());
        assert_eq!(policy.landed("o/other"), ["publish", "{sha}"]);
    }

    #[test]
    fn journal_forgets_entries_that_left_the_queue() {
        let mut journal = Journal::default();
        journal.scheduled.insert("o/r#1@h1".into(), "b1".into());
        journal.scheduled.insert("o/r#2@h2".into(), "b2".into());
        journal.scheduled.insert("p/q#1@h1".into(), "b1".into());
        let gone = journal.departed("o/r", &[entry(2, "h2")]);
        assert_eq!(gone.len(), 1);
        assert_eq!(
            (
                gone[0].number,
                gone[0].branch.as_str(),
                gone[0].head.as_str()
            ),
            (1, "b1", "h1")
        );
        assert_eq!(
            journal.scheduled.keys().cloned().collect::<Vec<_>>(),
            ["o/r#2@h2", "p/q#1@h1"]
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.json");
        journal.save(&path).unwrap();
        assert_eq!(Journal::load(&path).unwrap().scheduled, journal.scheduled);
    }

    #[test]
    fn landing_by_cfrg_starts_the_landed_hook_once() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            ..Fake::default()
        };
        fake.statuses.extend([green("h1")]);
        let mut hook = Hook::default();
        let mut journal = Journal::default();
        let report = step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert_eq!(hook.landed, ["h1"]);
        assert!(report
            .events
            .iter()
            .any(|e| e["event"] == "landed" && e["by"] == "cfrg"));
        step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert_eq!(hook.landed, ["h1"]);
    }

    #[test]
    fn a_merge_the_forge_did_alone_is_noticed_and_announced_once() {
        let mut fake = Fake {
            queue: vec![entry(1, "h1")],
            ..Fake::default()
        };
        let mut hook = Hook::default();
        let mut journal = Journal::default();
        // First pass schedules the pending entry.
        step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        // The forge then merges it natively when the checks turn green.
        fake.queue.clear();
        fake.merged.insert(1, "h1-merged".into());
        let report = step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert_eq!(hook.landed, ["h1-merged"]);
        assert!(report
            .events
            .iter()
            .any(|e| e["event"] == "landed" && e["by"] == "forge"));
        step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert_eq!(hook.landed, ["h1-merged"]);
        // An entry that was closed without merging announces nothing.
        let mut closed = Fake {
            queue: vec![entry(2, "h2")],
            ..Fake::default()
        };
        let mut hook = Hook::default();
        let mut journal = Journal::default();
        step(&mut closed, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        closed.queue.clear();
        step(&mut closed, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert!(hook.landed.is_empty());
    }

    fn ungated(queue: Vec<Entry>) -> Fake {
        Fake {
            queue,
            ungated: true,
            ..Fake::default()
        }
    }

    #[test]
    fn without_a_forge_gate_a_head_with_no_status_is_never_merged_or_scheduled() {
        let mut fake = ungated(vec![entry(1, "h1")]);
        let mut hook = Hook::default();
        let mut journal = Journal::default();
        for _ in 0..4 {
            let report = step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
            assert!(report.waiting);
        }
        // The native merge is cancelled once and never scheduled; nothing merged.
        assert_eq!(fake.log, ["unschedule 1"]);
        assert_eq!(hook.landed, Vec::<String>::new());
    }

    #[test]
    fn without_a_forge_gate_pending_waits_and_only_success_on_the_head_lands() {
        let mut fake = ungated(vec![entry(1, "h1")]);
        fake.statuses
            .insert("h1".into(), vec![("ci/crow/x".into(), State::Pending)]);
        let mut hook = Hook::default();
        let mut journal = Journal::default();
        let report = step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert!(report.waiting);
        assert_eq!(fake.log, ["unschedule 1"]);
        // A failure on the head is never merged either.
        fake.statuses
            .insert("h1".into(), vec![("ci/crow/x".into(), State::Failure)]);
        step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert_eq!(fake.log, ["unschedule 1"]);
        // Success on the exact head: cfrg itself merges it.
        fake.statuses.extend([green("h1")]);
        let report = step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert_eq!(fake.log, ["unschedule 1", "merge 1@h1"]);
        assert!(!report.waiting);
        assert_eq!(hook.landed, ["h1"]);
    }

    #[test]
    fn a_push_after_the_status_resets_the_wait_for_the_new_head() {
        let mut fake = ungated(vec![entry(1, "h1")]);
        fake.statuses.extend([green("h1")]);
        fake.moved = true;
        let mut hook = Hook::default();
        let mut journal = Journal::default();
        // The head moves between verification and merge: nothing is merged.
        let report = step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert!(report.waiting);
        assert!(report.events.iter().any(|e| e["reason"] == "head-moved"));
        assert!(!fake.log.iter().any(|l| l.starts_with("merge")));
        // The branch now carries a new commit that has no status: wait.
        fake.moved = false;
        fake.queue[0].head = "h2".into();
        let report = step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert!(report.waiting);
        assert!(!fake.log.iter().any(|l| l.starts_with("merge")));
        // Green on the OLD head still does not count for the new one.
        step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert!(!fake.log.iter().any(|l| l.starts_with("merge")));
        fake.statuses.extend([green("h2")]);
        step(&mut fake, &mut hook, &settings(), &mut journal, "o/r").unwrap();
        assert_eq!(fake.log.last().unwrap(), "merge 1@h2");
    }

    #[test]
    fn an_ungated_branch_cancels_every_queued_native_merge_once() {
        let mut fake = ungated(vec![entry(1, "h1"), entry(2, "h2")]);
        let mut journal = Journal::default();
        step(
            &mut fake,
            &mut Hook::default(),
            &settings(),
            &mut journal,
            "o/r",
        )
        .unwrap();
        step(
            &mut fake,
            &mut Hook::default(),
            &settings(),
            &mut journal,
            "o/r",
        )
        .unwrap();
        assert_eq!(fake.log, ["unschedule 1", "unschedule 2"]);
    }

    #[test]
    fn handing_over_to_serve_does_not_schedule_where_the_forge_does_not_gate() {
        let mut fake = ungated(vec![entry(1, "h1")]);
        let mut journal = Journal::default();
        enqueue(&mut fake, &settings(), &mut journal, "o/r", "b1").unwrap();
        enqueue(&mut fake, &settings(), &mut journal, "o/r", "b1").unwrap();
        assert_eq!(fake.log, ["enqueue b1", "unschedule 1", "enqueue b1"]);
    }
}
