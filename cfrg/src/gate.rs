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

use crate::Result;

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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    #[serde(default)]
    pub lane: Lane,
    /// Bare repository whose newest state is pushed (the srv git copy).
    pub source: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub budgets: Budgets,
    /// `org/repo` to its lane and source. A repository that is not listed is never pushed.
    pub repos: BTreeMap<String, RepoConfig>,
    /// Directory with ledger.jsonl and queue.json.
    pub state_dir: PathBuf,
    /// `<dir>/<org>` exists = that organisation is paused (flag probe, 429). Never removed by the gate.
    pub paused_dir: PathBuf,
    /// This directory exists = nothing is written to GitHub at all.
    pub kill_switch: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_str(&fs::read_to_string(path)?)?;
        for name in config.repos.keys() {
            split(name)?;
        }
        Ok(config)
    }
}

/// `org/repo` into its two parts; both must be plain names.
pub fn split(name: &str) -> Result<(&str, &str)> {
    let valid = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    match name.split_once('/') {
        Some((org, repo)) if valid(org) && valid(repo) && !repo.contains('/') => Ok((org, repo)),
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
            Ok(())
        }
        Kind::ActionsRun => {
            if count(world, DAY, Kind::ActionsRun, None) >= budgets.actions_runs_per_day {
                return Err(Deny::ActionsDaily);
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
        for kind in [Kind::Push, Kind::Create, Kind::ActionsRun] {
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
            .map(|i| create(NOW - 100 - i, "o", &format!("r{i}")))
            .collect();
        assert_eq!(
            verdict(&three, Lane::Bulk, "o", "n", Kind::Create),
            Err(Deny::NewReposOrgDaily)
        );
        assert_eq!(verdict(&three, Lane::Bulk, "p", "n", Kind::Create), Ok(()));
        let six: Vec<Entry> = (0..6)
            .map(|i| create(NOW - 100 - i, &format!("o{i}"), "r"))
            .collect();
        assert_eq!(
            verdict(&six, Lane::Bulk, "p", "n", Kind::Create),
            Err(Deny::NewReposDaily)
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
                    },
                ),
                (
                    "o/build-new".into(),
                    RepoConfig {
                        lane: Lane::Build,
                        source: "/x".into(),
                    },
                ),
                (
                    "o/bulk-new".into(),
                    RepoConfig {
                        lane: Lane::Bulk,
                        source: "/x".into(),
                    },
                ),
            ]),
            state_dir: "/s".into(),
            paused_dir: "/p".into(),
            kill_switch: "/k".into(),
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
        for bad in [
            "org",
            "org/",
            "/repo",
            "a/b/c",
            "../x/y",
            "o/.hidden",
            "o/r r",
            "o/r;x",
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
}
