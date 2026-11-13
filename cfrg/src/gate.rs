//! The GitHub write gate: the decisions that keep a burst of pushes from ever reaching GitHub.
//!
//! Everything here is pure: budgets, the ledger of past mutations, the queue order and the verdict
//! for one mutation. The clock is a parameter, the filesystem is only touched by [`Ledger::load`] and
//! [`Ledger::append`]. The executor (`cfrg gate flush`) asks [`decide`] immediately before every
//! mutation and records the mutation in the ledger first, so a crash can only over-count.
//!
//! Policy (ORG-MOVE-PLAN section 7): one overall budget per hour and day, one per organisation and
//! hour, a minimum gap per repository, a daily cap on new repositories and on Actions runs, a reserved
//! slice for build-relevant repositories (lane `build`) that a bulk fill cannot use, a pause directory
//! per organisation (written by the flag probe or the first 429) and a kill-switch directory.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use crate::{sync_hint::SyncHint, Result};

pub const HOUR: u64 = 3600;
pub const DAY: u64 = 86_400;

/// The budgets. Defaults are ZT1's constants; the file may override any of them.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Budgets {
    pub pushes_per_hour: u64,
    pub pushes_per_day: u64,
    pub org_pushes_per_hour: u64,
    pub repo_min_gap_secs: u64,
    pub new_repos_per_day_org: u64,
    pub new_repos_per_day: u64,
    pub actions_runs_per_day: u64,
    /// Repository deletions (`cfrg gate delete-repo`): at most this many per day, at least `delete_min_gap_secs` apart overall.
    pub deletes_per_day: u64,
    pub delete_min_gap_secs: u64,
    /// Repository creations (`cfrg gate create`): at least this long between two creations overall.
    pub create_min_gap_secs: u64,
    /// Slice of the hourly and daily push budget that only the build lane may use.
    pub build_reserve_per_hour: u64,
    pub build_reserve_per_day: u64,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            pushes_per_hour: 20,
            pushes_per_day: 120,
            org_pushes_per_hour: 8,
            repo_min_gap_secs: 600,
            new_repos_per_day_org: 3,
            new_repos_per_day: 6,
            actions_runs_per_day: 30,
            deletes_per_day: 6,
            delete_min_gap_secs: 600,
            create_min_gap_secs: 300,
            build_reserve_per_hour: 6,
            build_reserve_per_day: 30,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    /// Repositories whose GitHub Actions must build current code: first in line, own reserved slice.
    Build,
    #[default]
    Bulk,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Push,
    Create,
    /// Counted for every push to a repository whose Actions are enabled (lane build): the upper bound of runs our pushes start.
    ActionsRun,
    /// A repository deleted on GitHub (`cfrg gate delete-repo`).
    Delete,
}

/// One mutation (or reservation of one) that the gate has performed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub ts: u64,
    pub kind: Kind,
    pub org: String,
    pub repo: String,
    #[serde(default)]
    pub lane: Lane,
}

/// What a flush does with the Git LFS objects of one repository (operating model R4).
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Lfs {
    /// The default: the push carries the pointer files and nothing else, because the free plans limit LFS storage.
    #[default]
    Pointers,
    /// The explicit per-repository exception: after a successful push, the LFS objects the source references are uploaded too.
    Payload,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    #[serde(default)]
    pub lane: Lane,
    /// Bare repository whose newest state is pushed (the local git copy).
    pub source: PathBuf,
    /// LFS payloads are never replicated unless the repository declares `"lfs": "payload"` (R4). Default: pointers only.
    #[serde(default)]
    pub lfs: Lfs,
    /// Create the destination private. Copied from the primary; visibility is never changed afterwards.
    #[serde(default)]
    pub private: bool,
    /// Description for a destination the gate creates. Copied from the primary.
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub budgets: Budgets,
    /// Disable Actions for the first push into a fresh destination and re-enable them after it, so historical branches and tags
    /// cannot start runs from the workflow files those old commits still carry. Default true; only a test turns it off.
    #[serde(default = "quiet_fill_default")]
    pub quiet_fill: bool,
    /// Push and compare branches only: no tag is pushed, and tags are ignored when the source is compared with the destination.
    /// A tag on GitHub can start a release workflow, so an operator can hold every tag while branches keep flowing; the tags
    /// already at the destination stay as they are. Default false.
    #[serde(default)]
    pub hold_tags: bool,
    /// The most copy-only branches one push may delete (R3). A copy with more is held and reported as diverged: a person decides,
    /// because a source that lost branches by accident must never empty a destination. Default 10.
    #[serde(default = "max_prune_default")]
    pub max_prune: usize,
    /// `org/repo` to its lane and source. A repository that is not listed is never pushed.
    pub repos: BTreeMap<String, RepoConfig>,
    /// Directory with ledger.jsonl and queue.json.
    pub state_dir: PathBuf,
    /// `<dir>/<org>` exists = that organisation is paused (flag probe, 429). Never removed by the gate.
    pub paused_dir: PathBuf,
    /// This directory exists = nothing is written to GitHub at all.
    pub kill_switch: PathBuf,
    /// The tag hint to the forge bridge (operating model R45, [`crate::sync_hint`]): after a push that brought a tag to
    /// GitHub, `/hooks/sync` is told about each such tag. Absent = no hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_hint: Option<SyncHint>,
}

fn quiet_fill_default() -> bool {
    true
}

fn max_prune_default() -> usize {
    10
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_str(&fs::read_to_string(path)?)?;
        for name in config.repos.keys() {
            split(name)?;
        }
        if let Some(hint) = &config.sync_hint {
            hint.validate()?;
        }
        Ok(config)
    }
}

/// `org/repo` into its two parts; both must be plain names.
pub fn split(name: &str) -> Result<(&str, &str)> {
    // A repository part may start with a dot (GitHub's `.github` is one); an organisation part may not.
    let valid = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    match name.split_once('/') {
        Some((org, repo))
            if valid(org) && valid(repo) && !repo.contains('/') && !org.starts_with('.') =>
        {
            Ok((org, repo))
        }
        _ => Err(crate::failure(format!("Not an org/repo name: {name}"))),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deny {
    KillSwitch,
    OrgPaused,
    UnknownRepo,
    RepoGap { wait: u64 },
    OrgHourly,
    OverallHourly,
    OverallDaily,
    ActionsDaily,
    NewReposOrgDaily,
    NewReposDaily,
    DeleteGap { wait: u64 },
    DeleteDaily,
    CreateGap { wait: u64 },
}

impl Deny {
    pub fn text(&self) -> String {
        match self {
            Self::KillSwitch => "kill switch is set".into(),
            Self::OrgPaused => "organisation is paused".into(),
            Self::UnknownRepo => "repository is not listed in the gate configuration".into(),
            Self::RepoGap { wait } => format!("repository gap: {wait} s to wait"),
            Self::OrgHourly => "organisation hourly push budget is used".into(),
            Self::OverallHourly => "overall hourly push budget is used".into(),
            Self::OverallDaily => "overall daily push budget is used".into(),
            Self::ActionsDaily => "daily budget of Actions runs is used".into(),
            Self::NewReposOrgDaily => {
                "organisation's daily budget of new repositories is used".into()
            }
            Self::NewReposDaily => "overall daily budget of new repositories is used".into(),
            Self::DeleteGap { wait } => format!("delete pacing: {wait} s to wait"),
            Self::DeleteDaily => "daily budget of repository deletions is used".into(),
            Self::CreateGap { wait } => format!("creation gap: {wait} s to wait"),
        }
    }
}

/// Everything the verdict depends on besides the budgets.
pub struct World<'a> {
    pub now: u64,
    pub ledger: &'a [Entry],
    pub kill_switch_set: bool,
    pub org_paused: bool,
}

fn count(world: &World<'_>, window: u64, kind: Kind, org: Option<&str>) -> u64 {
    let since = world.now.saturating_sub(window);
    world
        .ledger
        .iter()
        .filter(|e| e.ts > since && e.kind == kind && org.is_none_or(|o| e.org == o))
        .count() as u64
}

fn count_lane(world: &World<'_>, window: u64, lane: Lane) -> u64 {
    let since = world.now.saturating_sub(window);
    world
        .ledger
        .iter()
        .filter(|e| e.ts > since && e.kind == Kind::Push && e.lane == lane)
        .count() as u64
}

/// The verdict for one mutation of `kind` on `org/repo` in `lane`.
pub fn decide(
    budgets: &Budgets,
    world: &World<'_>,
    lane: Lane,
    org: &str,
    repo: &str,
    kind: Kind,
) -> std::result::Result<(), Deny> {
    if world.kill_switch_set {
        return Err(Deny::KillSwitch);
    }
    if world.org_paused {
        return Err(Deny::OrgPaused);
    }
    match kind {
        Kind::Create => {
            if count(world, DAY, Kind::Create, Some(org)) >= budgets.new_repos_per_day_org {
                return Err(Deny::NewReposOrgDaily);
            }
            if count(world, DAY, Kind::Create, None) >= budgets.new_repos_per_day {
                return Err(Deny::NewReposDaily);
            }
            let last = world
                .ledger
                .iter()
                .filter(|e| e.kind == Kind::Create)
                .map(|e| e.ts)
                .max();
            if let Some(last) = last {
                let next = last.saturating_add(budgets.create_min_gap_secs);
                if world.now < next {
                    return Err(Deny::CreateGap {
                        wait: next - world.now,
                    });
                }
            }
            Ok(())
        }
        Kind::ActionsRun => {
            if count(world, DAY, Kind::ActionsRun, None) >= budgets.actions_runs_per_day {
                return Err(Deny::ActionsDaily);
            }
            Ok(())
        }
        Kind::Delete => {
            if count(world, DAY, Kind::Delete, None) >= budgets.deletes_per_day {
                return Err(Deny::DeleteDaily);
            }
            let last = world
                .ledger
                .iter()
                .filter(|e| e.kind == Kind::Delete)
                .map(|e| e.ts)
                .max();
            if let Some(last) = last {
                let next = last.saturating_add(budgets.delete_min_gap_secs);
                if world.now < next {
                    return Err(Deny::DeleteGap {
                        wait: next - world.now,
                    });
                }
            }
            Ok(())
        }
        Kind::Push => {
            let last = world
                .ledger
                .iter()
                .filter(|e| e.kind == Kind::Push && e.org == org && e.repo == repo)
                .map(|e| e.ts)
                .max();
            if let Some(last) = last {
                let next = last.saturating_add(budgets.repo_min_gap_secs);
                if world.now < next {
                    return Err(Deny::RepoGap {
                        wait: next - world.now,
                    });
                }
            }
            if count(world, HOUR, Kind::Push, Some(org)) >= budgets.org_pushes_per_hour {
                return Err(Deny::OrgHourly);
            }
            // The bulk lane stops before the end of the budget; the reserved slice is for the build lane only.
            let (hour_cap, day_cap) = match lane {
                Lane::Build => (budgets.pushes_per_hour, budgets.pushes_per_day),
                Lane::Bulk => (
                    budgets
                        .pushes_per_hour
                        .saturating_sub(budgets.build_reserve_per_hour),
                    budgets
                        .pushes_per_day
                        .saturating_sub(budgets.build_reserve_per_day),
                ),
            };
            // A bulk fill is measured against its own pushes only, so the build lane's use of the reserve never shrinks it further.
            let (used_hour, used_day) = match lane {
                Lane::Build => (
                    count(world, HOUR, Kind::Push, None),
                    count(world, DAY, Kind::Push, None),
                ),
                Lane::Bulk => (
                    count_lane(world, HOUR, Lane::Bulk),
                    count_lane(world, DAY, Lane::Bulk),
                ),
            };
            if used_hour >= hour_cap {
                return Err(Deny::OverallHourly);
            }
            if used_day >= day_cap {
                return Err(Deny::OverallDaily);
            }
            if lane == Lane::Build
                && count(world, DAY, Kind::ActionsRun, None) >= budgets.actions_runs_per_day
            {
                return Err(Deny::ActionsDaily);
            }
            Ok(())
        }
    }
}

/// A repository that has changes waiting.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Queued {
    pub repo: String,
    pub since: u64,
}

/// The order a flush works in: the build lane first, then the one that has waited longest.
pub fn order(config: &Config, queue: &[Queued]) -> Vec<Queued> {
    let lane_of = |name: &str| config.repos.get(name).map_or(Lane::Bulk, |r| r.lane);
    let mut sorted: Vec<Queued> = queue.to_vec();
    sorted.sort_by(|a, b| {
        lane_of(&a.repo)
            .cmp(&lane_of(&b.repo))
            .then(a.since.cmp(&b.since))
            .then(a.repo.cmp(&b.repo))
    });
    sorted
}

/// Coalescing: the queue holds "repo has changes", never commits. Queueing a repository that is already queued keeps its first time.
pub fn enqueue(queue: &mut Vec<Queued>, repo: &str, now: u64) {
    if !queue.iter().any(|q| q.repo == repo) {
        queue.push(Queued {
            repo: repo.into(),
            since: now,
        });
    }
}

/// The ledger: one JSON object per line, append-only.
pub struct Ledger {
    path: PathBuf,
}

impl Ledger {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("ledger.jsonl"),
        }
    }
    /// Entries newer than two days (older ones never matter to a budget).
    pub fn load(&self, now: u64) -> Result<Vec<Entry>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut entries = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let entry: Entry = serde_json::from_str(line)?;
            if entry.ts + 2 * DAY > now {
                entries.push(entry);
            }
        }
        Ok(entries)
    }
    pub fn append(&self, entry: &Entry) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", serde_json::to_string(entry)?)?;
        file.sync_all()?;
        Ok(())
    }
}

pub fn load_queue(state_dir: &Path) -> Result<Vec<Queued>> {
    match fs::read_to_string(state_dir.join("queue.json")) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

pub fn save_queue(state_dir: &Path, queue: &[Queued]) -> Result<()> {
    fs::create_dir_all(state_dir)?;
    let target = state_dir.join("queue.json");
    let partial = state_dir.join("queue.json.new");
    fs::write(&partial, serde_json::to_string_pretty(queue)?)?;
    fs::rename(partial, target)?;
    Ok(())
}

/// The forge router's namespace on a copy: the heads of its relay pull requests (ccid wiki "Forge-router" §4). The gate never
/// pushes into it, never counts it and never deletes in it.
pub const RELAY_PREFIX: &str = "refs/heads/cforge/";

/// The dependency bot's transient branches (`renovate/*`): bot activity, never pushed to a copy and removed from a copy that has
/// them (they would burn the daily push budget). The opposite direction of the relay namespace.
pub const BOT_PREFIX: &str = "refs/heads/renovate/";

/// What one flush would do to one destination, decided from the two ref maps (refname to object name, branches and tags only, the
/// relay namespace already left out). Everything that needs the object database is the caller's `descends`, so this stays pure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Refs the push creates or fast-forwards.
    pub updates: Vec<String>,
    /// Copy-only branches the push deletes (operating model R3: the copy matches the primary).
    pub prune: Vec<String>,
    /// Refs a push without force cannot bring to the destination.
    pub conflicts: Vec<(String, Hold)>,
    /// Refs on the destination that are left alone on purpose: copy-only tags, the relay namespace and the destination's default
    /// branch (GitHub refuses to delete it, which would fail the whole atomic push).
    pub kept: Vec<String>,
}

/// Why a repository is held back from a push.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Hold {
    /// The destination's branch is not an ancestor of the source's: git answers non-fast-forward. The copy has diverged.
    NonFastForward,
    /// A tag stands at another object on the destination; a push without force never moves a tag.
    MovedTag,
    /// More copy-only branches than `max_prune`: a person decides.
    PruneLimit,
    /// The destination refused this exact push (a `[rejected]` status line); it is not repeated until a ref it names changes.
    PushRejected,
}

impl Hold {
    pub fn text(self) -> &'static str {
        match self {
            Self::NonFastForward => "a branch is not a fast-forward",
            Self::MovedTag => "a tag stands at another object",
            Self::PruneLimit => "more copy-only branches than max_prune",
            Self::PushRejected => "the destination rejected the push",
        }
    }
}

/// `descends(theirs, ours)`: the destination's tip `theirs` is an ancestor of (or equal to) the source's tip `ours`.
pub fn plan(
    source: &BTreeMap<String, String>,
    destination: &BTreeMap<String, String>,
    default_branch: Option<&str>,
    descends: impl Fn(&str, &str) -> bool,
) -> Plan {
    let mut plan = Plan::default();
    for (name, ours) in source {
        // The relay's namespace is the forge router's: a primary branch of that name is not pushed.
        if name.starts_with(RELAY_PREFIX) || name.starts_with(BOT_PREFIX) {
            continue;
        }
        match destination.get(name) {
            None => plan.updates.push(name.clone()),
            Some(theirs) if theirs == ours => {}
            Some(_) if name.starts_with("refs/tags/") => {
                plan.conflicts.push((name.clone(), Hold::MovedTag));
            }
            Some(theirs) if descends(theirs, ours) => plan.updates.push(name.clone()),
            Some(_) => plan.conflicts.push((name.clone(), Hold::NonFastForward)),
        }
    }
    for name in destination
        .keys()
        .filter(|n| !source.contains_key(*n) || n.starts_with(BOT_PREFIX))
    {
        // Only a plain branch is ever deleted: never a tag, never the relay's namespace, never `refs/pull/*` or any other
        // namespace, never the destination's default branch.
        if name.starts_with("refs/heads/")
            && !name.starts_with(RELAY_PREFIX)
            && Some(name.as_str()) != default_branch
        {
            plan.prune.push(name.clone());
        } else {
            plan.kept.push(name.clone());
        }
    }
    plan
}

impl Plan {
    /// Nothing to push, nothing to delete and nothing in the way: the destination is in sync.
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.prune.is_empty() && self.conflicts.is_empty()
    }

    /// The reason to hold the repository before any push is spent, with the refs concerned.
    pub fn hold(&self, max_prune: usize) -> Option<(Hold, Vec<String>)> {
        if !self.conflicts.is_empty() {
            let kind = if self
                .conflicts
                .iter()
                .any(|(_, h)| *h == Hold::NonFastForward)
            {
                Hold::NonFastForward
            } else {
                Hold::MovedTag
            };
            return Some((
                kind,
                self.conflicts.iter().map(|(n, _)| n.clone()).collect(),
            ));
        }
        (self.prune.len() > max_prune).then(|| (Hold::PruneLimit, self.prune.clone()))
    }

    /// Identifies this exact push: the refs it changes with their source and destination objects. A rejected push is not repeated
    /// while the fingerprint is the same; any new commit on the primary or change on the destination makes it a different push.
    pub fn fingerprint(
        &self,
        source: &BTreeMap<String, String>,
        destination: &BTreeMap<String, String>,
    ) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        let object = |map: &BTreeMap<String, String>, name: &str| {
            map.get(name).cloned().unwrap_or_else(|| "-".into())
        };
        for name in &self.updates {
            hasher.update(format!(
                "+{name} {} {}\n",
                object(source, name),
                object(destination, name)
            ));
        }
        for name in &self.prune {
            hasher.update(format!("-{name} {}\n", object(destination, name)));
        }
        format!("{:x}", hasher.finalize())
    }
}

/// The most refs a [`Divergence`] lists (the total is kept beside them).
pub const DIVERGENCE_REFS: usize = 20;

/// A held repository: written by an executing flush into `diverged.json` of the state directory, shown by `cfrg gate status`
/// (the nightwatch and the status page read it there). It clears itself when a flush finds the cause gone.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Divergence {
    /// First flush that found this hold (kept while the same hold persists), and the latest one.
    pub since: u64,
    pub seen: u64,
    pub hold: Hold,
    pub refs: Vec<String>,
    pub refs_total: usize,
    pub fingerprint: String,
}

impl Divergence {
    pub fn new(
        previous: Option<&Divergence>,
        now: u64,
        hold: Hold,
        refs: Vec<String>,
        fingerprint: String,
    ) -> Self {
        let since = previous.filter(|p| p.hold == hold).map_or(now, |p| p.since);
        let refs_total = refs.len();
        Self {
            since,
            seen: now,
            hold,
            refs: refs.into_iter().take(DIVERGENCE_REFS).collect(),
            refs_total,
            fingerprint,
        }
    }
}

/// `org/repo` to its hold; absent file = nothing is held.
pub fn load_diverged(state_dir: &Path) -> Result<BTreeMap<String, Divergence>> {
    match fs::read_to_string(state_dir.join("diverged.json")) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

pub fn save_diverged(state_dir: &Path, held: &BTreeMap<String, Divergence>) -> Result<()> {
    fs::create_dir_all(state_dir)?;
    let target = state_dir.join("diverged.json");
    let partial = state_dir.join("diverged.json.new");
    fs::write(&partial, serde_json::to_string_pretty(held)?)?;
    fs::rename(partial, target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_000_000;

    fn push(ts: u64, org: &str, repo: &str, lane: Lane) -> Entry {
        Entry {
            ts,
            kind: Kind::Push,
            org: org.into(),
            repo: repo.into(),
            lane,
        }
    }
    fn world(ledger: &[Entry]) -> World<'_> {
        World {
            now: NOW,
            ledger,
            kill_switch_set: false,
            org_paused: false,
        }
    }
    fn verdict(
        ledger: &[Entry],
        lane: Lane,
        org: &str,
        repo: &str,
        kind: Kind,
    ) -> std::result::Result<(), Deny> {
        decide(&Budgets::default(), &world(ledger), lane, org, repo, kind)
    }

    #[test]
    fn an_empty_ledger_allows_a_push() {
        assert_eq!(verdict(&[], Lane::Bulk, "o", "r", Kind::Push), Ok(()));
    }

    #[test]
    fn the_kill_switch_and_a_paused_org_stop_everything() {
        let b = Budgets::default();
        let mut w = world(&[]);
        w.kill_switch_set = true;
        for kind in [Kind::Push, Kind::Create, Kind::ActionsRun, Kind::Delete] {
            assert_eq!(
                decide(&b, &w, Lane::Build, "o", "r", kind),
                Err(Deny::KillSwitch)
            );
        }
        w.kill_switch_set = false;
        w.org_paused = true;
        assert_eq!(
            decide(&b, &w, Lane::Build, "o", "r", Kind::Push),
            Err(Deny::OrgPaused)
        );
    }

    #[test]
    fn a_repository_waits_its_gap() {
        let ledger = [push(NOW - 100, "o", "r", Lane::Bulk)];
        assert_eq!(
            verdict(&ledger, Lane::Bulk, "o", "r", Kind::Push),
            Err(Deny::RepoGap { wait: 500 })
        );
        assert_eq!(
            verdict(&ledger, Lane::Bulk, "o", "other", Kind::Push),
            Ok(())
        );
        let old = [push(NOW - 601, "o", "r", Lane::Bulk)];
        assert_eq!(verdict(&old, Lane::Bulk, "o", "r", Kind::Push), Ok(()));
    }

    #[test]
    fn an_org_has_its_own_hourly_budget() {
        let ledger: Vec<Entry> = (0..8)
            .map(|i| push(NOW - 1000 - i, "o", &format!("r{i}"), Lane::Build))
            .collect();
        assert_eq!(
            verdict(&ledger, Lane::Build, "o", "new", Kind::Push),
            Err(Deny::OrgHourly)
        );
        assert_eq!(
            verdict(&ledger, Lane::Build, "p", "new", Kind::Push),
            Ok(())
        );
    }

    #[test]
    fn the_bulk_lane_cannot_use_the_reserved_slice_but_the_build_lane_can() {
        // 14 bulk pushes in the last hour over several orgs: the bulk cap is 20 - 6 = 14.
        let ledger: Vec<Entry> = (0..14)
            .map(|i| {
                push(
                    NOW - 1000 - i,
                    &format!("o{}", i % 7),
                    &format!("r{i}"),
                    Lane::Bulk,
                )
            })
            .collect();
        assert_eq!(
            verdict(&ledger, Lane::Bulk, "x", "r", Kind::Push),
            Err(Deny::OverallHourly)
        );
        assert_eq!(verdict(&ledger, Lane::Build, "x", "r", Kind::Push), Ok(()));
    }

    #[test]
    fn the_build_lane_stops_at_the_full_budget() {
        let ledger: Vec<Entry> = (0..20)
            .map(|i| {
                push(
                    NOW - 1000 - i,
                    &format!("o{}", i % 7),
                    &format!("r{i}"),
                    Lane::Build,
                )
            })
            .collect();
        assert_eq!(
            verdict(&ledger, Lane::Build, "x", "r", Kind::Push),
            Err(Deny::OverallHourly)
        );
    }

    #[test]
    fn the_daily_budget_counts_a_day_and_forgets_older_pushes() {
        let ledger: Vec<Entry> = (0..120)
            .map(|i| push(NOW - 4000 - i * 600, &format!("o{i}"), "r", Lane::Build))
            .collect();
        // all inside the last day (120 * 600 s = 20 h): the day is spent
        assert_eq!(
            verdict(&ledger, Lane::Build, "x", "r", Kind::Push),
            Err(Deny::OverallDaily)
        );
        let old: Vec<Entry> = (0..120)
            .map(|i| push(NOW - DAY - 10 - i, &format!("o{i}"), "r", Lane::Build))
            .collect();
        assert_eq!(verdict(&old, Lane::Build, "x", "r", Kind::Push), Ok(()));
    }

    #[test]
    fn new_repositories_are_capped_per_org_and_overall() {
        let create = |ts, org: &str, repo: &str| Entry {
            ts,
            kind: Kind::Create,
            org: org.into(),
            repo: repo.into(),
            lane: Lane::Bulk,
        };
        let three: Vec<Entry> = (0..3)
            .map(|i| create(NOW - 1000 - i, "o", &format!("r{i}")))
            .collect();
        assert_eq!(
            verdict(&three, Lane::Bulk, "o", "n", Kind::Create),
            Err(Deny::NewReposOrgDaily)
        );
        assert_eq!(verdict(&three, Lane::Bulk, "p", "n", Kind::Create), Ok(()));
        let six: Vec<Entry> = (0..6)
            .map(|i| create(NOW - 1000 - i, &format!("o{i}"), "r"))
            .collect();
        assert_eq!(
            verdict(&six, Lane::Bulk, "p", "n", Kind::Create),
            Err(Deny::NewReposDaily)
        );
    }

    #[test]
    fn repository_creations_keep_their_gap() {
        let create = |ts| Entry {
            ts,
            kind: Kind::Create,
            org: "o".into(),
            repo: "r".into(),
            lane: Lane::Bulk,
        };
        assert_eq!(
            verdict(&[create(NOW - 100)], Lane::Bulk, "o", "n", Kind::Create),
            Err(Deny::CreateGap { wait: 200 })
        );
        assert_eq!(
            verdict(&[create(NOW - 400)], Lane::Bulk, "o", "n", Kind::Create),
            Ok(())
        );
    }

    #[test]
    fn actions_runs_are_capped_and_stop_build_pushes() {
        let runs: Vec<Entry> = (0..30)
            .map(|i| Entry {
                ts: NOW - 100 - i,
                kind: Kind::ActionsRun,
                org: "o".into(),
                repo: format!("r{i}"),
                lane: Lane::Build,
            })
            .collect();
        assert_eq!(
            verdict(&runs, Lane::Build, "o", "r", Kind::Push),
            Err(Deny::ActionsDaily)
        );
        // a bulk push starts no run on GitHub (Actions are off on bulk mirrors)
        assert_eq!(verdict(&runs, Lane::Bulk, "o", "r", Kind::Push), Ok(()));
    }

    #[test]
    fn the_queue_coalesces_and_the_build_lane_goes_first() {
        let config = Config {
            budgets: Budgets::default(),
            repos: BTreeMap::from([
                (
                    "o/bulk-old".into(),
                    RepoConfig {
                        lane: Lane::Bulk,
                        source: "/x".into(),
                        lfs: Lfs::Pointers,
                        private: false,
                        description: String::new(),
                    },
                ),
                (
                    "o/build-new".into(),
                    RepoConfig {
                        lane: Lane::Build,
                        source: "/x".into(),
                        lfs: Lfs::Pointers,
                        private: false,
                        description: String::new(),
                    },
                ),
                (
                    "o/bulk-new".into(),
                    RepoConfig {
                        lane: Lane::Bulk,
                        source: "/x".into(),
                        lfs: Lfs::Pointers,
                        private: false,
                        description: String::new(),
                    },
                ),
            ]),
            state_dir: "/s".into(),
            paused_dir: "/p".into(),
            kill_switch: "/k".into(),
            quiet_fill: true,
            hold_tags: false,
            max_prune: 10,
            sync_hint: None,
        };
        let mut queue = Vec::new();
        enqueue(&mut queue, "o/bulk-new", 30);
        enqueue(&mut queue, "o/bulk-old", 10);
        enqueue(&mut queue, "o/build-new", 40);
        enqueue(&mut queue, "o/bulk-old", 99); // N more commits: still one entry, first time kept
        assert_eq!(queue.len(), 3);
        let names: Vec<String> = order(&config, &queue).into_iter().map(|q| q.repo).collect();
        assert_eq!(names, ["o/build-new", "o/bulk-old", "o/bulk-new"]);
    }

    #[test]
    fn names_are_plain() {
        assert!(split("org/repo").is_ok());
        assert!(split("org/repo.git-1_x").is_ok());
        assert!(split("o/.github").is_ok());
        for bad in [
            "org", "org/", "/repo", "a/b/c", "../x/y", "o/.", "o/..", ".o/r", "o/r r", "o/r;x",
        ] {
            assert!(split(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_ledger_is_durable_and_forgets_old_entries() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(dir.path());
        ledger
            .append(&push(NOW - 3 * DAY, "o", "old", Lane::Bulk))
            .unwrap();
        ledger
            .append(&push(NOW - 5, "o", "r", Lane::Build))
            .unwrap();
        let loaded = ledger.load(NOW).unwrap();
        assert_eq!(loaded, [push(NOW - 5, "o", "r", Lane::Build)]);
        assert!(Ledger::new(&dir.path().join("none"))
            .load(NOW)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_queue_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_queue(dir.path()).unwrap().is_empty());
        let queue = vec![Queued {
            repo: "o/r".into(),
            since: 7,
        }];
        save_queue(dir.path(), &queue).unwrap();
        assert_eq!(load_queue(dir.path()).unwrap(), queue);
    }

    #[test]
    fn deletions_are_paced_one_slot_apart_and_capped_per_day() {
        let b = Budgets::default();
        let entry = |ts| Entry {
            ts,
            kind: Kind::Delete,
            org: "o".into(),
            repo: "r".into(),
            lane: Lane::Bulk,
        };
        assert_eq!(verdict(&[], Lane::Bulk, "o", "x", Kind::Delete), Ok(()));
        // A deletion of ANY repository holds the next one back for the gap.
        let w = World {
            now: 1_000_000,
            ledger: &[entry(1_000_000 - 100)],
            kill_switch_set: false,
            org_paused: false,
        };
        assert_eq!(
            decide(&b, &w, Lane::Bulk, "p", "y", Kind::Delete),
            Err(Deny::DeleteGap { wait: 500 })
        );
        let spaced: Vec<Entry> = (0..6).map(|i| entry(1_000_000 - 1000 - i * 700)).collect();
        let w = World {
            now: 1_000_000,
            ledger: &spaced,
            kill_switch_set: false,
            org_paused: false,
        };
        assert_eq!(
            decide(&b, &w, Lane::Bulk, "p", "y", Kind::Delete),
            Err(Deny::DeleteDaily)
        );
    }

    fn refs_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(n, o)| ((*n).to_string(), (*o).to_string()))
            .collect()
    }

    #[test]
    fn a_plan_creates_fast_forwards_and_prunes_copy_only_branches() {
        let source = refs_of(&[
            ("refs/heads/main", "b2"),
            ("refs/heads/new", "n1"),
            ("refs/tags/v1", "t1"),
        ]);
        let destination = refs_of(&[
            ("refs/heads/main", "b1"),
            ("refs/heads/stale", "s1"),
            ("refs/tags/v1", "t1"),
            ("refs/tags/old", "t0"),
        ]);
        let plan = plan(&source, &destination, None, |theirs, ours| {
            (theirs, ours) == ("b1", "b2")
        });
        assert_eq!(plan.updates, ["refs/heads/main", "refs/heads/new"]);
        assert_eq!(plan.prune, ["refs/heads/stale"]);
        assert!(plan.conflicts.is_empty());
        assert_eq!(
            plan.kept,
            ["refs/tags/old"],
            "a copy-only tag is never deleted"
        );
        assert!(plan.hold(10).is_none());
        assert!(!plan.is_empty());
    }

    #[test]
    fn nothing_to_do_is_in_sync_even_with_copy_only_tags() {
        let source = refs_of(&[("refs/heads/main", "b1")]);
        let destination = refs_of(&[("refs/heads/main", "b1"), ("refs/tags/gh-only", "t")]);
        let plan = plan(&source, &destination, None, |_, _| true);
        assert!(plan.is_empty(), "{plan:?}");
        assert_eq!(plan.kept, ["refs/tags/gh-only"]);
    }

    #[test]
    fn a_branch_that_is_not_a_fast_forward_and_a_moved_tag_are_conflicts() {
        let source = refs_of(&[("refs/heads/main", "b2"), ("refs/tags/v1", "t2")]);
        let destination = refs_of(&[("refs/heads/main", "b1"), ("refs/tags/v1", "t1")]);
        let plan = plan(&source, &destination, None, |_, _| false);
        assert_eq!(
            plan.conflicts,
            [
                ("refs/heads/main".to_string(), Hold::NonFastForward),
                ("refs/tags/v1".to_string(), Hold::MovedTag)
            ]
        );
        let (kind, refs) = plan.hold(10).unwrap();
        assert_eq!(kind, Hold::NonFastForward);
        assert_eq!(refs, ["refs/heads/main", "refs/tags/v1"]);
        // A moved tag alone is reported as that, even when every branch could fast-forward.
        let tag_only = refs_of(&[("refs/tags/v1", "t2")]);
        let plan = super::plan(
            &tag_only,
            &refs_of(&[("refs/tags/v1", "t1")]),
            None,
            |_, _| true,
        );
        assert_eq!(plan.hold(10).unwrap().0, Hold::MovedTag);
    }

    #[test]
    fn pruning_never_reaches_the_relay_namespace_other_namespaces_or_the_default_branch() {
        // A primary branch inside the relay's namespace is not pushed either.
        let source = refs_of(&[("refs/heads/main", "b1"), ("refs/heads/cforge/own", "o1")]);
        let destination = refs_of(&[
            ("refs/heads/main", "b1"),
            ("refs/heads/cforge/gitlab-7", "r1"),
            ("refs/pull/1/head", "p1"),
            ("refs/heads/legacy", "l1"),
            ("refs/heads/stale", "s1"),
        ]);
        // Even if a caller forgot to leave the relay namespace out, the plan keeps it.
        let plan = plan(&source, &destination, Some("refs/heads/legacy"), |_, _| {
            true
        });
        assert!(plan.updates.is_empty(), "{plan:?}");
        assert_eq!(plan.prune, ["refs/heads/stale"]);
        assert_eq!(
            plan.kept,
            [
                "refs/heads/cforge/gitlab-7",
                "refs/heads/legacy",
                "refs/pull/1/head"
            ]
        );
    }

    #[test]
    fn the_bots_branches_are_never_pushed_and_are_pruned_from_a_copy() {
        let source = refs_of(&[("refs/heads/main", "b2"), ("refs/heads/renovate/x", "n1")]);
        let destination = refs_of(&[("refs/heads/main", "b1"), ("refs/heads/renovate/old", "n0")]);
        let plan = plan(&source, &destination, None, |_, _| true);
        assert_eq!(plan.updates, ["refs/heads/main"]);
        assert_eq!(plan.prune, ["refs/heads/renovate/old"]);
    }

    #[test]
    fn too_many_copy_only_branches_hold_the_repository() {
        let source = refs_of(&[("refs/heads/main", "b1")]);
        let mut destination = refs_of(&[("refs/heads/main", "b1")]);
        for i in 0..3 {
            destination.insert(format!("refs/heads/gone-{i}"), "x".into());
        }
        let plan = plan(&source, &destination, None, |_, _| true);
        assert!(plan.hold(3).is_none());
        let (kind, refs) = plan.hold(2).unwrap();
        assert_eq!(kind, Hold::PruneLimit);
        assert_eq!(refs.len(), 3);
    }

    #[test]
    fn the_fingerprint_changes_with_either_side_and_not_with_kept_refs() {
        let source = refs_of(&[("refs/heads/main", "b2")]);
        let destination = refs_of(&[("refs/heads/main", "b1"), ("refs/heads/stale", "s1")]);
        let print = |s: &BTreeMap<String, String>, d: &BTreeMap<String, String>| {
            plan(s, d, None, |_, _| true).fingerprint(s, d)
        };
        let base = print(&source, &destination);
        assert_eq!(base, print(&source, &destination), "stable");
        assert_ne!(
            base,
            print(&refs_of(&[("refs/heads/main", "b3")]), &destination)
        );
        let mut moved = destination.clone();
        moved.insert("refs/heads/main".into(), "b0".into());
        assert_ne!(base, print(&source, &moved));
        let mut tagged = destination.clone();
        tagged.insert("refs/tags/gh-only".into(), "t".into());
        assert_eq!(base, print(&source, &tagged));
    }

    #[test]
    fn a_divergence_keeps_its_first_time_while_the_same_hold_lasts_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let first = Divergence::new(
            None,
            100,
            Hold::NonFastForward,
            vec!["refs/heads/main".into()],
            "f".into(),
        );
        let again = Divergence::new(
            Some(&first),
            200,
            Hold::NonFastForward,
            vec!["refs/heads/main".into()],
            "f".into(),
        );
        assert_eq!((again.since, again.seen), (100, 200));
        let other = Divergence::new(Some(&first), 300, Hold::PushRejected, vec![], "g".into());
        assert_eq!(
            other.since, 300,
            "another kind of hold starts its own clock"
        );
        let many: Vec<String> = (0..DIVERGENCE_REFS + 5)
            .map(|i| format!("refs/tags/t{i}"))
            .collect();
        let long = Divergence::new(None, 1, Hold::MovedTag, many, "h".into());
        assert_eq!(
            (long.refs.len(), long.refs_total),
            (DIVERGENCE_REFS, DIVERGENCE_REFS + 5)
        );
        assert!(load_diverged(dir.path()).unwrap().is_empty());
        let held = BTreeMap::from([("o/r".to_string(), again)]);
        save_diverged(dir.path(), &held).unwrap();
        assert_eq!(load_diverged(dir.path()).unwrap(), held);
    }
}
