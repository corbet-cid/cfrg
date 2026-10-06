//! Exact-mirror access reconciliation across forges.
//!
//! Pure policy: identity maps, role normalisation and the three-way merge.
//! No network access, no filesystem access and no credentials here. The
//! `forge access` CLI feeds this module with collector-supplied JSON
//! snapshots, prints the planned API calls, and advances the local baseline
//! state. Live writes run through [`UnwiredTransport`], which refuses every
//! call: wire a reviewed transport before touching a provider API.
//!
//! See `docs/access-sync.md` for the semantics contract.
use crate::model::{Forge, Level};
use crate::{failure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

/// Prefix for canonicalised personal-namespace organisations. An org whose
/// name is a mapped handle (for example a personal org held under different
/// handles on different forges) compares as `person:<id>` on every forge.
const PERSON_PREFIX: &str = "person:";

/// Refuse to reason about larger inputs; oversized inventories fail closed.
const MAX_PEOPLE: usize = 1024;
const MAX_SNAPSHOT_GRANTS: usize = 16384;
const MAX_BASELINE_GRANTS: usize = 65536;

/// One forge's role vocabulary. Implemented once per adapter (`cfgj`, `cglb`,
/// `cbkt`, `cghb`); the merge stays forge-independent and receives the
/// tables from the CLI. The accepted vocabularies are documented in
/// `docs/access-sync.md`: the collector must record a documented effective
/// permission, never a guessed one.
pub trait RoleMap {
    fn normalize_role(&self, role: &str) -> Result<Level>;
}

/// One forge's grant-write API shapes. Implemented once per adapter; the
/// planner builds calls through this table instead of naming any forge.
/// Endpoint templates are UNVERIFIED (see `docs/access-sync.md`); every
/// produced call carries `endpoint_verified: false` until a live transport
/// proves it.
pub trait Grants {
    fn write_call(
        &self,
        team: Option<&str>,
        repo: Option<&str>,
        level: Option<Level>,
        org: &str,
        handle: &str,
        is_new: bool,
    ) -> Call;
}

/// Role vocabularies for every forge, supplied by the CLI from the adapters.
pub struct RoleMaps {
    pub github: &'static dyn RoleMap,
    pub forgejo: &'static dyn RoleMap,
    pub gitlab: &'static dyn RoleMap,
    pub bitbucket: &'static dyn RoleMap,
}

impl RoleMaps {
    pub fn get(&self, forge: Forge) -> &'static dyn RoleMap {
        match forge {
            Forge::Github => self.github,
            Forge::Forgejo => self.forgejo,
            Forge::Gitlab => self.gitlab,
            Forge::Bitbucket => self.bitbucket,
        }
    }
}

/// Grant-write API shapes for every forge, supplied by the CLI.
pub struct GrantTable {
    pub github: &'static dyn Grants,
    pub forgejo: &'static dyn Grants,
    pub gitlab: &'static dyn Grants,
    pub bitbucket: &'static dyn Grants,
}

impl GrantTable {
    pub fn get(&self, forge: Forge) -> &'static dyn Grants {
        match forge {
            Forge::Github => self.github,
            Forge::Forgejo => self.forgejo,
            Forge::Gitlab => self.gitlab,
            Forge::Bitbucket => self.bitbucket,
        }
    }
}

/// One observed grant, addressed by forge handle with the forge-native role.
/// Exactly one of `team` or `repo` must be set.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedGrant {
    pub handle: String,
    pub org: String,
    #[serde(default)]
    pub team: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    pub role: String,
    /// Forge event/audit timestamp when the collector supplies one. Falls
    /// back to the snapshot observation time, then to the run time.
    #[serde(default)]
    pub changed_at: Option<i64>,
}

/// One collector-supplied observation snapshot for a single forge.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedFile {
    pub forge: Forge,
    pub observed_at: i64,
    #[serde(default)]
    pub grants: Vec<ObservedGrant>,
}

/// Declarative person identity as parsed from TOML: a stable person id maps
/// to at most one handle per forge. Call [`IdentityMap::maps`] for the
/// validated form used by the merge. Handles only; entries with emails or
/// secrets are rejected during validation.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityMap {
    pub schema: u32,
    #[serde(default)]
    pub people: BTreeMap<String, BTreeMap<String, String>>,
}

/// Validated identity maps: forward (person to handle) and reverse (handle
/// to person) lookups. Matching during the merge uses only these maps.
#[derive(Debug, Clone)]
pub struct ValidatedMaps {
    people: BTreeMap<String, BTreeMap<String, String>>,
    reverse: BTreeMap<(String, String), String>,
}

fn valid_name(value: &str) -> bool {
    cqlt::valid_login(value)
}

impl IdentityMap {
    pub fn maps(&self) -> Result<ValidatedMaps> {
        if self.schema != 1 {
            return Err(failure("Identity map requires schema = 1"));
        }
        if self.people.is_empty() || self.people.len() > MAX_PEOPLE {
            return Err(failure(
                "Identity map must name between 1 and 1024 people; refusing an empty sync",
            ));
        }
        let mut validated = ValidatedMaps {
            people: BTreeMap::new(),
            reverse: BTreeMap::new(),
        };
        for (person, handles) in &self.people {
            if !valid_name(person) {
                return Err(failure(format!("Invalid person id: {person}")));
            }
            if handles.is_empty() {
                return Err(failure(format!("Person {person} maps to no forge account")));
            }
            for (forge, handle) in handles {
                let kind = Forge::parse(forge).map_err(|_| {
                    failure(format!("Person {person} names unsupported forge: {forge}"))
                })?;
                if !valid_name(handle) {
                    return Err(failure(format!(
                        "Person {person} has an invalid {forge} handle; handles only, never emails"
                    )));
                }
                // Handles match case-insensitively on every supported forge,
                // so both indexes use lowercase while forward handles keep
                // their declared spelling for API calls.
                if validated
                    .reverse
                    .insert(
                        (kind.as_str().to_owned(), handle.to_lowercase()),
                        person.clone(),
                    )
                    .is_some()
                {
                    return Err(failure(format!(
                        "Handle {handle} on {forge} is claimed twice"
                    )));
                }
                validated
                    .people
                    .entry(person.clone())
                    .or_default()
                    .insert(kind.as_str().to_owned(), handle.clone());
            }
        }
        Ok(validated)
    }
}

impl ValidatedMaps {
    pub fn person_for(&self, forge: &str, handle: &str) -> Option<&str> {
        self.reverse
            .get(&(forge.to_owned(), handle.to_lowercase()))
            .map(String::as_str)
    }

    pub fn handle_for(&self, person: &str, forge: &str) -> Option<&str> {
        self.people
            .get(person)
            .and_then(|handles| handles.get(forge))
            .map(String::as_str)
    }

    pub fn knows(&self, person: &str) -> bool {
        self.people.contains_key(person)
    }

    /// Every forge where at least one person holds a mapped account.
    pub fn forges(&self) -> BTreeSet<String> {
        self.reverse
            .keys()
            .map(|(forge, _)| forge.clone())
            .collect()
    }
}

/// One canonicalised grant in the baseline state file, keyed by person.
/// `org` holds either a shared organisation name or a `person:<id>`
/// personal namespace. Exactly one of `team` or `repo` is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredGrant {
    pub person: String,
    pub org: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub level: Level,
    pub at: i64,
}

/// Last-synced baseline state, grouped per forge. The merge compares each
/// forge observation against its group; `apply` advances the file only from
/// converged plans or explicit `--initialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub schema: u32,
    #[serde(default)]
    pub forges: BTreeMap<String, Vec<StoredGrant>>,
}

/// Planner inputs that vary per run: frozen forges (read but never
/// written), the operator person id (never touched), and the run time used
/// when a snapshot carries no usable timestamp.
pub struct PlanOptions {
    pub frozen: Vec<Forge>,
    pub operator: Option<String>,
    pub run_at: u64,
}

/// One exact intended API call. `endpoint_verified` is false for every
/// endpoint in this draft: templates still need live verification against
/// the official references in `docs/access-sync.md`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Call {
    pub forge: String,
    pub operation: String,
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    pub lookups: Vec<String>,
    pub endpoint_verified: bool,
}

/// One writable action: the mirrored grant, change or revocation for a
/// single forge. `level` is the desired outcome (absent for revocations).
#[derive(Debug, Clone, Serialize)]
pub struct PlannedAction {
    pub person: String,
    pub org: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Forge where the winning event was observed.
    pub source_forge: String,
    pub event_time: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    pub call: Call,
}

/// A desired write the planner refused, with the call that would have run.
#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub person: String,
    pub org: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub reason: String,
    pub call: Call,
}

/// One divergent event behind a conflict. `level` is absent for revocations.
#[derive(Debug, Clone, Serialize)]
pub struct EventView {
    pub forge: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    pub time: i64,
}

/// The same person+target changed differently on at least two forges. A
/// resolved conflict carries the latest change mirrored everywhere plus the
/// overwritten outcomes; an exact timestamp tie picks no winner.
#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub person: String,
    pub org: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub resolved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<EventView>,
    pub overwritten: Vec<EventView>,
}

/// Cross-forge divergence with no events since the baseline. Reported
/// without writes: with no change information there is nothing to mirror.
/// A present grant versus absence elsewhere is tolerated (partial rollout);
/// two present levels that disagree are drift.
#[derive(Debug, Clone, Serialize)]
pub struct Drift {
    pub person: String,
    pub org: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub levels: BTreeMap<String, Level>,
}

/// An observed account nobody claims in the identity map. Reported, never
/// touched.
#[derive(Debug, Clone, Serialize)]
pub struct UnmappedAccount {
    pub forge: String,
    pub handle: String,
    pub org: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub role: String,
}

/// A mirror target the planner could not verify: the person holds a mapped
/// account on a non-frozen forge that supplied no snapshot while a change
/// needed mirroring there. Reported; the forge is never assumed.
#[derive(Debug, Clone, Serialize)]
pub struct UnmappedTarget {
    pub forge: String,
    pub person: String,
    pub org: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub reason: String,
}

/// The offline plan: writable actions with exact calls, plus every refusal,
/// conflict, drift and unmapped report. `complete` means no action remains.
#[derive(Debug, Clone, Serialize)]
pub struct AccessPlan {
    pub baseline_present: bool,
    pub complete: bool,
    pub actions: Vec<PlannedAction>,
    pub skipped_frozen: Vec<Skipped>,
    pub skipped_protected: Vec<Skipped>,
    pub conflicts: Vec<Conflict>,
    pub drift: Vec<Drift>,
    pub unmapped_accounts: Vec<UnmappedAccount>,
    pub unmapped_targets: Vec<UnmappedTarget>,
    /// Human notes from observation (ambiguous namespaces, empty snapshots).
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Canonicalised observations for one run, grouped per forge.
#[derive(Debug, Clone, Default)]
pub struct World {
    pub forges: BTreeMap<Forge, BTreeMap<Target, WorldGrant>>,
}

/// One canonical grant: person-keyed target, normalised level, event time.
#[derive(Debug, Clone)]
pub struct WorldGrant {
    pub level: Level,
    pub at: i64,
}

/// Canonical mirror target. `org` is lowercase, or a `person:<id>`
/// personal namespace; exactly one of `team`/`repo` is set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Target {
    pub person: String,
    pub org: String,
    pub team: Option<String>,
    pub repo: Option<String>,
}

fn canonical_org(maps: &ValidatedMaps, org: &str, notes: &mut Vec<String>) -> String {
    let lowered = org.to_ascii_lowercase();
    let mut owners = BTreeSet::new();
    for person in maps.people.keys() {
        if let Some(handles) = maps.people.get(person) {
            if handles
                .values()
                .any(|h| h == org || h.to_ascii_lowercase() == lowered)
            {
                owners.insert(person.clone());
            }
        }
    }
    if owners.len() == 1 {
        let person = owners.into_iter().next().unwrap_or_default();
        return format!("{PERSON_PREFIX}{person}");
    }
    if owners.len() > 1 {
        notes.push(format!(
            "Ambiguous personal namespace {org}; keeping the literal name"
        ));
    }
    lowered
}

fn check_names(
    handle: &str,
    org: &str,
    team: &Option<String>,
    repo: &Option<String>,
) -> Result<()> {
    if !valid_name(handle) || !valid_name(org) {
        return Err(failure("Observed an invalid handle or organisation"));
    }
    match (team, repo) {
        (Some(team), None) if valid_name(team) => Ok(()),
        (None, Some(repo)) if valid_name(repo) => Ok(()),
        _ => Err(failure("Observed grants need exactly one of team or repo")),
    }
}

/// Canonical mirror target shared by observations and the baseline, so a
/// hand-written baseline compares exactly like a collector snapshot.
fn canonical_target(
    maps: &ValidatedMaps,
    person: &str,
    org: &str,
    team: &Option<String>,
    repo: &Option<String>,
    notes: &mut Vec<String>,
) -> Result<Target> {
    if !maps.knows(person) {
        return Err(failure(format!(
            "Grant names {person} with no mapped account; prune the baseline explicitly"
        )));
    }
    match (team, repo) {
        (Some(team), None) if valid_name(team) => (),
        (None, Some(repo)) if valid_name(repo) => (),
        _ => return Err(failure("Grants need exactly one of team or repo")),
    }
    Ok(Target {
        person: person.to_owned(),
        org: canonical_org(maps, org, notes),
        team: team.clone().map(|t| t.to_ascii_lowercase()),
        repo: repo.clone().map(|r| r.to_ascii_lowercase()),
    })
}

/// Canonicalised observation: the person-keyed world, accounts without an
/// identity mapping, human notes, and the observed forge names.
pub type Observation = (World, Vec<UnmappedAccount>, Vec<String>, Vec<String>);

/// Canonicalise snapshots into person-keyed worlds. Returns the [`Observation`].
pub fn observe(
    maps: &ValidatedMaps,
    files: &[ObservedFile],
    roles: &RoleMaps,
) -> Result<Observation> {
    let mut seen_forges = BTreeSet::new();
    for file in files {
        if !seen_forges.insert(file.forge) {
            return Err(failure(format!(
                "Duplicate snapshot for forge {}",
                file.forge.as_str()
            )));
        }
        if file.observed_at < 0 {
            return Err(failure("Snapshot observation time must be a Unix time"));
        }
        if file.grants.len() > MAX_SNAPSHOT_GRANTS {
            return Err(failure(format!(
                "Forge {} exceeds reconciliation limits",
                file.forge.as_str()
            )));
        }
    }
    let mut world = World::default();
    let mut unmapped = Vec::new();
    let mut notes = Vec::new();
    for file in files {
        if file.grants.is_empty() {
            notes.push(format!("Forge {} reported no grants", file.forge.as_str()));
        }
        let mut seen = BTreeSet::new();
        for grant in &file.grants {
            check_names(&grant.handle, &grant.org, &grant.team, &grant.repo)?;
            if let Some(stamp) = grant.changed_at {
                if stamp < 0 {
                    return Err(failure("Grant event time must be a Unix time"));
                }
            }
            let shape = (
                grant.handle.clone(),
                grant.org.clone(),
                grant.team.clone(),
                grant.repo.clone(),
            );
            if !seen.insert(shape) {
                return Err(failure(format!(
                    "Forge {} observed the same grant twice",
                    file.forge.as_str()
                )));
            }
            let Some(person) = maps.person_for(file.forge.as_str(), &grant.handle) else {
                unmapped.push(UnmappedAccount {
                    forge: file.forge.as_str().into(),
                    handle: grant.handle.clone(),
                    org: grant.org.clone(),
                    team: grant.team.clone(),
                    repo: grant.repo.clone(),
                    role: grant.role.clone(),
                });
                continue;
            };
            let level = roles
                .get(file.forge)
                .normalize_role(&grant.role)
                .map_err(|_| {
                    failure(format!(
                        "Forge {} grant for {} carries unknown role {:?}; refusing to guess",
                        file.forge.as_str(),
                        grant.handle,
                        grant.role
                    ))
                })?;
            let at = grant.changed_at.unwrap_or(file.observed_at);
            let target = canonical_target(
                maps,
                person,
                &grant.org,
                &grant.team,
                &grant.repo,
                &mut notes,
            )?;
            if world
                .forges
                .entry(file.forge)
                .or_default()
                .insert(target, WorldGrant { level, at })
                .is_some()
            {
                return Err(failure("Conflicting observations for one mapped grant"));
            }
        }
    }
    let observed = seen_forges.iter().map(|f| f.as_str().into()).collect();
    Ok((world, unmapped, notes, observed))
}

/// Render the observed world as a baseline state file.
pub fn snapshot_world(world: &World) -> Baseline {
    let mut forges: BTreeMap<String, Vec<StoredGrant>> = BTreeMap::new();
    for (forge, grants) in &world.forges {
        let mut stored: Vec<StoredGrant> = grants
            .iter()
            .map(|(target, grant)| StoredGrant {
                person: target.person.clone(),
                org: target.org.clone(),
                team: target.team.clone(),
                repo: target.repo.clone(),
                level: grant.level,
                at: grant.at,
            })
            .collect();
        stored.sort_by(|a, b| {
            (&a.person, &a.org, &a.team, &a.repo).cmp(&(&b.person, &b.org, &b.team, &b.repo))
        });
        forges.insert(forge.as_str().into(), stored);
    }
    Baseline { schema: 1, forges }
}

fn valid_canonical_org(org: &str) -> bool {
    if valid_name(org) {
        return true;
    }
    org.strip_prefix(PERSON_PREFIX)
        .is_some_and(|person| !person.is_empty() && valid_name(person))
}

/// Rebuild the comparison world from a baseline state file, canonicalising
/// exactly like [`observe`] so hand-written baselines compare correctly.
pub fn world_from_baseline(maps: &ValidatedMaps, baseline: &Baseline) -> Result<World> {
    if baseline.schema != 1 {
        return Err(failure("Baseline state requires schema = 1"));
    }
    let mut total = 0;
    let mut world = World::default();
    let mut notes = Vec::new();
    for (name, grants) in &baseline.forges {
        let forge = Forge::parse(name)?;
        total += grants.len();
        if total > MAX_BASELINE_GRANTS {
            return Err(failure("Baseline state exceeds reconciliation limits"));
        }
        for grant in grants {
            if !valid_name(&grant.person) || !valid_canonical_org(&grant.org) || grant.at < 0 {
                return Err(failure(format!(
                    "Baseline names an invalid grant on {name}; prune the baseline explicitly"
                )));
            }
            if maps.handle_for(&grant.person, forge.as_str()).is_none() {
                return Err(failure(format!(
                    "Baseline names {} on {name} with no mapped account; prune the baseline explicitly",
                    grant.person
                )));
            }
            let target = canonical_target(
                maps,
                &grant.person,
                &grant.org,
                &grant.team,
                &grant.repo,
                &mut notes,
            )
            .map_err(|_| {
                failure(format!(
                    "Baseline names an invalid grant on {name}; prune the baseline explicitly"
                ))
            })?;
            if world
                .forges
                .entry(forge)
                .or_default()
                .insert(
                    target,
                    WorldGrant {
                        level: grant.level,
                        at: grant.at,
                    },
                )
                .is_some()
            {
                return Err(failure(format!(
                    "Baseline grants the same target twice on {name}"
                )));
            }
        }
    }
    Ok(world)
}

/// Look up one canonical target in a comparison world. A free function with
/// explicit lifetimes: the closure version captured the loop-owned target by
/// reference while returning an argument-derived reference, which lifetime
/// inference rejects.
fn lookup<'a>(
    forge: Forge,
    table: &'a BTreeMap<Forge, BTreeMap<Target, WorldGrant>>,
    target: &Target,
) -> Option<&'a WorldGrant> {
    table.get(&forge).and_then(|grants| grants.get(target))
}

/// Three-way merge over the baseline and one snapshot per forge.
///
/// Every change since the baseline becomes an event; each event is mirrored
/// to every other non-frozen forge where the person holds a mapped account,
/// so all writable forges converge on the identical grant set. The latest
/// change to the same person+target wins and the overwritten change is
/// reported; an exact timestamp tie picks no winner. Revocations propagate:
/// a plain union of current states is wrong.
pub fn plan(
    maps: &IdentityMap,
    baseline: Option<&Baseline>,
    files: &[ObservedFile],
    options: &PlanOptions,
    roles: &RoleMaps,
    grants: &GrantTable,
) -> Result<AccessPlan> {
    let validated = maps.maps()?;
    if files.is_empty() {
        return Err(failure("Provide at least one observed forge snapshot"));
    }
    if let Some(operator) = &options.operator {
        if !validated.knows(operator) {
            return Err(failure(format!("Unknown operator person id: {operator}")));
        }
    }
    let run_at =
        i64::try_from(options.run_at).map_err(|_| failure("Run time does not fit a Unix time"))?;
    // Snapshots without their own observation time inherit the run time,
    // completing the changed_at -> observed_at -> run time fallback chain.
    let stamped: Vec<ObservedFile> = files
        .iter()
        .map(|file| {
            let mut owned = file.clone();
            if owned.observed_at == 0 {
                owned.observed_at = run_at;
            }
            owned
        })
        .collect();
    let (world, unmapped_accounts, notes, _observed_names) = observe(&validated, &stamped, roles)?;
    let files = &stamped;
    let mut report = AccessPlan {
        baseline_present: baseline.is_some(),
        complete: false,
        actions: Vec::new(),
        skipped_frozen: Vec::new(),
        skipped_protected: Vec::new(),
        conflicts: Vec::new(),
        drift: Vec::new(),
        unmapped_accounts,
        unmapped_targets: Vec::new(),
        notes,
    };
    let Some(previous) = baseline else {
        // A missing baseline plans nothing; the operator bootstraps it
        // explicitly with `apply --initialize`.
        return Ok(report);
    };
    let old = world_from_baseline(&validated, previous)?;
    let frozen: BTreeSet<Forge> = options.frozen.iter().copied().collect();
    let observed: BTreeSet<Forge> = files.iter().map(|f| f.forge).collect();

    let mut keys: BTreeSet<Target> = BTreeSet::new();
    for grants in world.forges.values() {
        keys.extend(grants.keys().cloned());
    }
    for grants in old.forges.values() {
        keys.extend(grants.keys().cloned());
    }

    for target in keys {
        let mut mapped: Vec<Forge> = Vec::new();
        for forge in validated.forges() {
            let kind = Forge::parse(&forge)?;
            if validated.handle_for(&target.person, &forge).is_some() {
                mapped.push(kind);
            }
        }
        // Events on observed mapped forges only. An unobserved forge
        // contributes no event: absence of evidence is not evidence.
        let mut events: Vec<(Forge, Option<Level>, Option<Level>, i64)> = Vec::new();
        for forge in &mapped {
            let before = lookup(*forge, &old.forges, &target).map(|g| g.level);
            let entry = lookup(*forge, &world.forges, &target);
            let (after, time) = match entry {
                Some(found) => (Some(found.level), found.at),
                None => {
                    let stamp = files
                        .iter()
                        .find(|f| f.forge == *forge)
                        .map(|f| f.observed_at)
                        .unwrap_or(0);
                    (None, stamp)
                }
            };
            if observed.contains(forge) && after != before {
                events.push((*forge, before, after, time));
            }
        }
        if events.is_empty() {
            // No change information: report genuine contradictions, write
            // nothing. A present grant versus absence elsewhere is
            // tolerated as a partial rollout.
            let mut levels: BTreeMap<String, Level> = BTreeMap::new();
            for forge in &mapped {
                if observed.contains(forge) {
                    if let Some(found) = lookup(*forge, &world.forges, &target) {
                        levels.insert(forge.as_str().into(), found.level);
                    }
                }
            }
            if levels.values().collect::<BTreeSet<_>>().len() > 1 {
                report.drift.push(Drift {
                    person: target.person.clone(),
                    org: target.org.clone(),
                    team: target.team.clone(),
                    repo: target.repo.clone(),
                    levels,
                });
            }
            continue;
        }
        let latest = events
            .iter()
            .map(|(_, _, _, time)| *time)
            .max()
            .unwrap_or(0);
        let tied: Vec<_> = events
            .iter()
            .filter(|(_, _, _, time)| *time == latest)
            .collect();
        let tied_levels: BTreeSet<Option<Level>> = tied.iter().map(|(_, _, new, _)| *new).collect();
        let views = |items: &[&(Forge, Option<Level>, Option<Level>, i64)]| {
            items
                .iter()
                .map(|(forge, _, new, time)| EventView {
                    forge: forge.as_str().into(),
                    level: *new,
                    time: *time,
                })
                .collect::<Vec<_>>()
        };
        if tied_levels.len() > 1 {
            report.conflicts.push(Conflict {
                person: target.person.clone(),
                org: target.org.clone(),
                team: target.team.clone(),
                repo: target.repo.clone(),
                resolved: false,
                winner: None,
                overwritten: views(&tied),
            });
            continue;
        }
        let winner: Option<Level> = tied_levels.iter().next().copied().flatten();
        let Some(winner_event) = tied.first() else {
            continue;
        };
        let winner_forge = winner_event.0;
        let distinct: BTreeSet<Option<Level>> = events.iter().map(|(_, _, new, _)| *new).collect();
        if distinct.len() > 1 {
            let mut overwritten: Vec<EventView> = views(
                &events
                    .iter()
                    .filter(|(_, _, new, _)| *new != winner)
                    .collect::<Vec<_>>(),
            );
            overwritten.sort_by(|a, b| (&a.forge, a.time).cmp(&(&b.forge, b.time)));
            report.conflicts.push(Conflict {
                person: target.person.clone(),
                org: target.org.clone(),
                team: target.team.clone(),
                repo: target.repo.clone(),
                resolved: true,
                winner: Some(EventView {
                    forge: winner_forge.as_str().into(),
                    level: winner,
                    time: latest,
                }),
                overwritten,
            });
        }
        for forge in &mapped {
            let before = lookup(*forge, &old.forges, &target).map(|g| g.level);
            let entry = lookup(*forge, &world.forges, &target);
            let after = entry.map(|g| g.level);
            let handle = validated
                .handle_for(&target.person, forge.as_str())
                .unwrap_or_default()
                .to_owned();
            // Executable calls address the forge-local namespace: a
            // personal `person:<id>` org renders as that person's handle on
            // this forge. Reports elsewhere keep the canonical form.
            let display_org = match target.org.strip_prefix(PERSON_PREFIX) {
                Some(_) => validated
                    .handle_for(&target.person, forge.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                None => target.org.clone(),
            };
            if options.operator.as_ref() == Some(&target.person) {
                // The operator's own access is never touched on any forge.
                let diverged = if observed.contains(forge) {
                    after != winner
                } else {
                    before != winner
                };
                if diverged {
                    report.skipped_protected.push(Skipped {
                        person: target.person.clone(),
                        org: display_org.clone(),
                        team: target.team.clone(),
                        repo: target.repo.clone(),
                        reason: "operator".into(),
                        call: grants.get(*forge).write_call(
                            target.team.as_deref(),
                            target.repo.as_deref(),
                            winner,
                            &display_org,
                            &handle,
                            after.is_none(),
                        ),
                    });
                }
                continue;
            }
            if frozen.contains(forge) {
                // Compare against current reality when observed: a stale
                // baseline entry alone must not report a phantom skip.
                let relevant = if observed.contains(forge) {
                    after
                } else {
                    before
                };
                if relevant != winner {
                    report.skipped_frozen.push(Skipped {
                        person: target.person.clone(),
                        org: display_org.clone(),
                        team: target.team.clone(),
                        repo: target.repo.clone(),
                        reason: "frozen-forge".into(),
                        call: grants.get(*forge).write_call(
                            target.team.as_deref(),
                            target.repo.as_deref(),
                            winner,
                            &display_org,
                            &handle,
                            after.is_none(),
                        ),
                    });
                }
                continue;
            }
            if !observed.contains(forge) {
                if before != winner {
                    report.unmapped_targets.push(UnmappedTarget {
                        forge: forge.as_str().into(),
                        person: target.person.clone(),
                        org: display_org.clone(),
                        team: target.team.clone(),
                        repo: target.repo.clone(),
                        reason: "forge-unobserved".into(),
                    });
                }
                continue;
            }
            if after == winner {
                continue;
            }
            // Personal-namespace owners are never removed or downgraded: an
            // admin grant under the person's own namespace keeps admin.
            let demotes_owner = target.org == format!("{PERSON_PREFIX}{}", target.person)
                && entry.is_some_and(|found| found.level == Level::Admin)
                && winner != Some(Level::Admin);
            if demotes_owner {
                report.skipped_protected.push(Skipped {
                    person: target.person.clone(),
                    org: display_org.clone(),
                    team: target.team.clone(),
                    repo: target.repo.clone(),
                    reason: "owner-admin-protected".into(),
                    call: grants.get(*forge).write_call(
                        target.team.as_deref(),
                        target.repo.as_deref(),
                        winner,
                        &display_org,
                        &handle,
                        after.is_none(),
                    ),
                });
                continue;
            }
            report.actions.push(PlannedAction {
                person: target.person.clone(),
                org: display_org.clone(),
                team: target.team.clone(),
                repo: target.repo.clone(),
                source_forge: winner_forge.as_str().into(),
                event_time: latest,
                level: winner,
                call: grants.get(*forge).write_call(
                    target.team.as_deref(),
                    target.repo.as_deref(),
                    winner,
                    &display_org,
                    &handle,
                    after.is_none(),
                ),
            });
        }
    }

    report.actions.sort_by(|a, b| {
        (&a.call.forge, &a.person, &a.org, &a.team, &a.repo).cmp(&(
            &b.call.forge,
            &b.person,
            &b.org,
            &b.team,
            &b.repo,
        ))
    });
    report.skipped_frozen.sort_by(|a, b| {
        (&a.call.forge, &a.person, &a.org).cmp(&(&b.call.forge, &b.person, &b.org))
    });
    report.skipped_protected.sort_by(|a, b| {
        (&a.call.forge, &a.person, &a.org).cmp(&(&b.call.forge, &b.person, &b.org))
    });
    report.complete = report.actions.is_empty()
        && report.conflicts.iter().all(|c| c.resolved)
        && report.drift.is_empty();
    Ok(report)
}

/// Provider error with an optional HTTP status. Auth (401/403), plan-limit
/// (402) and throttle (429) statuses stop the whole run; any other failure skips the rest of that forge while
/// the remaining forges still run.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl ApiError {
    /// Failure without a usable status (transport refusal, network error).
    pub fn refused(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
        }
    }

    /// Failure with a provider HTTP status.
    pub fn status(status: u16, message: impl Into<String>) -> Self {
        Self {
            status: Some(status),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(f, "forge API error {status}: {}", self.message),
            None => write!(f, "forge API error: {}", self.message),
        }
    }
}

impl std::error::Error for ApiError {}

/// Live forge writes behind the paced apply engine. Unwired in this draft:
/// every call is refused without network I/O, so `apply` fails closed with
/// the baseline untouched. Wire a reviewed live transport here before
/// performing provider writes.
pub trait Transport {
    /// Execute one planned call, returning the provider status when known.
    fn execute(&mut self, call: &Call) -> std::result::Result<(), ApiError>;
}

/// The draft transport: refuses every write.
pub struct UnwiredTransport;

impl Transport for UnwiredTransport {
    fn execute(&mut self, _call: &Call) -> std::result::Result<(), ApiError> {
        Err(ApiError::refused(
            "Live forge transport is unwired; refusing writes",
        ))
    }
}

/// One failed or skipped call in the apply report.
#[derive(Debug, Clone, Serialize)]
pub struct FailedCall {
    pub call: Call,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub message: String,
}

/// Outcome of driving a plan through a transport.
#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub attempted: usize,
    pub succeeded: Vec<Call>,
    pub failed: Vec<FailedCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
    /// Baseline to persist: advanced per forge only after all of that
    /// forge's writes succeeded and every event it originated fully
    /// propagated, so the next run retries exactly the remainder.
    pub next: Baseline,
    pub complete: bool,
}

/// Drive a plan through a transport with pacing between calls.
///
/// Actions run grouped by forge in plan order. The first 401/402/403/429 stops
/// the entire run; any other error skips the rest of that forge and
/// continues with the next forge. Calls never attempted after a stop or a
/// forge halt are recorded as failures so their keys never advance.
pub fn apply_plan(
    plan: &AccessPlan,
    transport: &mut dyn Transport,
    pace: Duration,
    maps: &ValidatedMaps,
    observed: &BTreeSet<String>,
    previous: &Baseline,
) -> ApplyReport {
    let mut by_forge: BTreeMap<String, Vec<&PlannedAction>> = BTreeMap::new();
    for action in &plan.actions {
        by_forge
            .entry(action.call.forge.clone())
            .or_default()
            .push(action);
    }
    let mut succeeded: Vec<Call> = Vec::new();
    let mut failed: Vec<FailedCall> = Vec::new();
    let mut stopped: Option<String> = None;
    let mut attempted = 0;
    let mut first = true;
    'run: for actions in by_forge.values() {
        for action in actions {
            if stopped.is_some() {
                break 'run;
            }
            if !first && !pace.is_zero() {
                std::thread::sleep(pace);
            }
            first = false;
            attempted += 1;
            match transport.execute(&action.call) {
                Ok(()) => succeeded.push(action.call.clone()),
                Err(error) => {
                    // Stop on the first auth, throttle or plan-limit
                    // signal and record the halt; anything else fails
                    // only its own call.
                    let halt = error.status.is_some_and(|status| {
                        status == 401 || status == 402 || status == 403 || status == 429
                    });
                    if halt {
                        stopped = Some(error.message.clone());
                    }
                    failed.push(FailedCall {
                        call: action.call.clone(),
                        status: error.status,
                        message: error.message,
                    });
                    if halt {
                        break 'run;
                    }
                    break;
                }
            }
        }
    }
    // Never-attempted calls are failures too: their keys must not advance.
    for action in &plan.actions {
        if !succeeded.contains(&action.call) && !failed.iter().any(|item| item.call == action.call)
        {
            let message = if stopped.is_some() {
                "run stopped before this call"
            } else {
                "forge halted before this call"
            };
            failed.push(FailedCall {
                call: action.call.clone(),
                status: None,
                message: message.into(),
            });
        }
    }
    let failed_calls: BTreeSet<&Call> = failed.iter().map(|item| &item.call).collect();
    let mut blocked_keys = BTreeSet::new();
    for skipped in plan
        .skipped_frozen
        .iter()
        .map(|item| (&item.person, &item.org, &item.team, &item.repo))
        .chain(
            plan.skipped_protected
                .iter()
                .map(|item| (&item.person, &item.org, &item.team, &item.repo)),
        )
        .chain(
            plan.unmapped_targets
                .iter()
                .map(|item| (&item.person, &item.org, &item.team, &item.repo)),
        )
    {
        blocked_keys.insert(skipped);
    }
    // Winners per canonical key. Every action for one key mirrors the same
    // outcome by construction.
    type TargetKey<'a> = (
        &'a String,
        &'a String,
        &'a Option<String>,
        &'a Option<String>,
    );
    type WinningVote = (Option<Level>, i64);
    let mut winners: BTreeMap<TargetKey<'_>, WinningVote> = BTreeMap::new();
    for action in &plan.actions {
        winners
            .entry((&action.person, &action.org, &action.team, &action.repo))
            .or_insert((action.level, action.event_time));
    }
    type BaseKey = (String, String, String, Option<String>, Option<String>);
    let mut levels: BTreeMap<BaseKey, (Level, i64)> = BTreeMap::new();
    for (name, grants) in &previous.forges {
        for grant in grants {
            levels.insert(
                (
                    name.clone(),
                    grant.person.clone(),
                    grant.org.clone(),
                    grant.team.clone(),
                    grant.repo.clone(),
                ),
                (grant.level, grant.at),
            );
        }
    }
    for ((person, org, team, repo), (winner, at)) in &winners {
        let blocked = plan.actions.iter().any(|action| {
            &action.person == *person
                && &action.org == *org
                && &action.team == *team
                && &action.repo == *repo
                && failed_calls.contains(&action.call)
        }) || blocked_keys.contains(&(*person, *org, *team, *repo));
        if blocked {
            continue;
        }
        // Fully mirrored: every mapped observed forge converges on the
        // winner. Forges already carrying it need no write; unobserved
        // forges are never assumed.
        for forge in observed {
            if maps.handle_for(person, forge).is_none() {
                continue;
            }
            let key = (
                (*forge).clone(),
                (*person).clone(),
                (*org).clone(),
                (*team).clone(),
                (*repo).clone(),
            );
            match winner {
                Some(level) => {
                    levels.insert(key, (*level, *at));
                }
                None => {
                    levels.remove(&key);
                }
            }
        }
    }
    let mut forges: BTreeMap<String, Vec<StoredGrant>> = BTreeMap::new();
    for ((forge, person, org, team, repo), (level, at)) in levels {
        forges.entry(forge).or_default().push(StoredGrant {
            person,
            org,
            team,
            repo,
            level,
            at,
        });
    }
    for grants in forges.values_mut() {
        grants.sort_by(|a, b| {
            (&a.person, &a.org, &a.team, &a.repo).cmp(&(&b.person, &b.org, &b.team, &b.repo))
        });
    }
    ApplyReport {
        attempted,
        succeeded,
        failed,
        stopped,
        next: Baseline { schema: 1, forges },
        complete: false,
    }
    .with_completion()
}

impl ApplyReport {
    fn with_completion(mut self) -> Self {
        self.complete = self.failed.is_empty() && self.stopped.is_none();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test vocabulary covering every role used in planner fixtures.
    /// Per-forge tables live with the adapters and are tested there.
    struct TestRoles;
    impl RoleMap for TestRoles {
        fn normalize_role(&self, role: &str) -> Result<Level> {
            match role {
                "read" => Ok(Level::Read),
                "write" | "developer" => Ok(Level::Write),
                "admin" | "owner" => Ok(Level::Admin),
                _ => Err(failure(format!("Unknown test role: {role}"))),
            }
        }
    }

    /// Deterministic stand-in for adapter call templates: routing, forge,
    /// operation and revocation shape are planner facts; exact paths, bodies
    /// and methods are adapter facts tested in each adapter crate.
    struct EchoGrants(&'static str);
    impl Grants for EchoGrants {
        fn write_call(
            &self,
            team: Option<&str>,
            repo: Option<&str>,
            level: Option<Level>,
            org: &str,
            handle: &str,
            _is_new: bool,
        ) -> Call {
            Call {
                forge: self.0.into(),
                operation: if level.is_none() {
                    "revoke"
                } else {
                    "set-level"
                }
                .into(),
                method: if level.is_none() { "DELETE" } else { "PUT" }.into(),
                path: format!(
                    "{}/{}/{}/{}",
                    self.0,
                    org,
                    team.or(repo).unwrap_or_default(),
                    handle
                ),
                body: String::new(),
                lookups: Vec::new(),
                endpoint_verified: false,
            }
        }
    }

    static GITHUB_ROLES: TestRoles = TestRoles;
    static FORGEJO_ROLES: TestRoles = TestRoles;
    static GITLAB_ROLES: TestRoles = TestRoles;
    static BITBUCKET_ROLES: TestRoles = TestRoles;
    static GITHUB_GRANTS: EchoGrants = EchoGrants("github");
    static FORGEJO_GRANTS: EchoGrants = EchoGrants("forgejo");
    static GITLAB_GRANTS: EchoGrants = EchoGrants("gitlab");
    static BITBUCKET_GRANTS: EchoGrants = EchoGrants("bitbucket");

    fn roles() -> RoleMaps {
        RoleMaps {
            github: &GITHUB_ROLES,
            forgejo: &FORGEJO_ROLES,
            gitlab: &GITLAB_ROLES,
            bitbucket: &BITBUCKET_ROLES,
        }
    }

    fn grants() -> GrantTable {
        GrantTable {
            github: &GITHUB_GRANTS,
            forgejo: &FORGEJO_GRANTS,
            gitlab: &GITLAB_GRANTS,
            bitbucket: &BITBUCKET_GRANTS,
        }
    }

    fn maps() -> ValidatedMaps {
        let parsed: IdentityMap = toml::from_str(
            "schema = 1\n\
             [people.alice]\n\
             github = \"alice-gh\"\n\
             forgejo = \"alice-fj\"\n\
             gitlab = \"alice-gl\"\n\
             [people.bruno]\n\
             github = \"bruno-gh\"\n\
             forgejo = \"bruno-fj\"\n\
             bitbucket = \"bruno-bb\"\n",
        )
        .unwrap();
        parsed.maps().unwrap()
    }

    fn options() -> PlanOptions {
        PlanOptions {
            frozen: vec![Forge::Github],
            operator: None,
            run_at: 1_760_000_000,
        }
    }

    /// One observed grant row: handle, org, team, repo, role, change time.
    type GrantRow<'a> = (
        &'a str,
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        &'a str,
        Option<i64>,
    );

    fn observed(forge: Forge, at: i64, grants: Vec<GrantRow<'_>>) -> ObservedFile {
        ObservedFile {
            forge,
            observed_at: at,
            grants: grants
                .into_iter()
                .map(
                    |(handle, org, team, repo, role, changed_at)| ObservedGrant {
                        handle: handle.into(),
                        org: org.into(),
                        team: team.map(str::to_owned),
                        repo: repo.map(str::to_owned),
                        role: role.into(),
                        changed_at,
                    },
                )
                .collect(),
        }
    }

    fn team_grant<'a>(handle: &'a str, role: &'a str, at: Option<i64>) -> GrantRow<'a> {
        (handle, "acme", Some("dev"), None, role, at)
    }

    fn world_with(
        maps: &ValidatedMaps,
        files: &[ObservedFile],
    ) -> (World, BTreeMap<Target, BTreeMap<String, Level>>) {
        let (world, _, _, _) = observe(maps, files, &roles()).unwrap();
        let mut flat = BTreeMap::new();
        for (forge, grants) in &world.forges {
            for (target, grant) in grants {
                flat.entry(target.clone())
                    .or_insert_with(BTreeMap::new)
                    .insert(forge.as_str().into(), grant.level);
            }
        }
        (world, flat)
    }

    #[test]
    fn identity_map_rejects_guesses() {
        let parsed: IdentityMap =
            toml::from_str("schema = 1\n[people.alice]\nforgejo = \"alice-fj\"\n").unwrap();
        assert_eq!(
            parsed.maps().unwrap().person_for("forgejo", "alice-fj"),
            Some("alice")
        );
        for bad in [
            "schema = 1\n[people.alice]\nplan9 = \"alice-p9\"\n",
            "schema = 1\n[people.alice]\nforgejo = \"alice-fj\"\n[people.eve]\nforgejo = \"alice-fj\"\n",
            "schema = 1\n[people.alice]\nforgejo = \"alice@example.org\"\n",
            "schema = 1\n[people.alice]\n",
            "schema = 2\n[people.alice]\nforgejo = \"alice-fj\"\n",
            "schema = 1\n",
        ] {
            let parsed: IdentityMap = toml::from_str(bad).unwrap_or(IdentityMap {
                schema: 0,
                people: BTreeMap::new(),
            });
            assert!(parsed.maps().is_err(), "{bad}");
        }
        assert!(Forge::parse("plan9").is_err());
    }

    #[test]
    fn personal_namespaces_canonicalise_across_forges() {
        let maps = maps();
        let files = vec![observed(
            Forge::Gitlab,
            100,
            vec![("alice-gl", "alice-fj", Some("dev"), None, "developer", None)],
        )];
        let (_, flat) = world_with(&maps, &files);
        assert_eq!(flat.len(), 1);
        let (target, levels) = flat.into_iter().next().unwrap();
        assert_eq!(target.person, "alice");
        assert_eq!(target.org, "person:alice");
        assert_eq!(levels.get("gitlab"), Some(&Level::Write));
    }

    #[test]
    fn personal_namespace_mirrors_resolve_local_handles() {
        // GitHub carries a grant under Alice's personal org; Forgejo and
        // GitLab must receive it under her handles there. Executable calls
        // never carry the canonical `person:` form.
        let files = vec![
            observed(
                Forge::Github,
                100,
                vec![(
                    "alice-gh",
                    "alice-gh",
                    None,
                    Some("widget"),
                    "admin",
                    Some(50),
                )],
            ),
            observed(Forge::Forgejo, 100, vec![]),
            observed(Forge::Gitlab, 100, vec![]),
        ];
        let previous = Baseline {
            schema: 1,
            forges: BTreeMap::new(),
        };
        let report = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert_eq!(report.actions.len(), 2);
        let mut orgs: Vec<(&str, &str)> = report
            .actions
            .iter()
            .map(|action| (action.call.forge.as_str(), action.org.as_str()))
            .collect();
        orgs.sort();
        assert_eq!(orgs, vec![("forgejo", "alice-fj"), ("gitlab", "alice-gl")]);
        assert!(report
            .actions
            .iter()
            .all(|action| !action.call.path.contains("person:")));
        assert!(report.skipped_frozen.is_empty());
    }

    #[test]
    fn grant_mirrors_to_mapped_forges_never_to_frozen() {
        let files = vec![
            observed(Forge::Github, 100, vec![]),
            observed(
                Forge::Forgejo,
                100,
                vec![team_grant("alice-fj", "write", Some(90))],
            ),
            observed(Forge::Gitlab, 100, vec![]),
        ];
        let previous = Baseline {
            schema: 1,
            forges: BTreeMap::new(),
        };
        let report = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert!(report.baseline_present);
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].call.forge, "gitlab");
        assert_eq!(report.actions[0].call.operation, "set-level");
        // Exact paths, bodies and methods are adapter facts (see cglb);
        // the planner owns routing, forge and operation.
        assert_eq!(report.actions[0].call.method, "PUT");
        assert_eq!(report.actions[0].call.path, "gitlab/acme/dev/alice-gl");
        assert!(report.actions[0].call.body.is_empty());
        assert!(!report.actions[0].call.endpoint_verified);
        assert_eq!(report.skipped_frozen.len(), 1);
        assert_eq!(report.skipped_frozen[0].call.forge, "github");
        assert!(!report.complete);
    }

    fn maps_toml() -> IdentityMap {
        toml::from_str(
            "schema = 1\n\
             [people.alice]\n\
             github = \"alice-gh\"\n\
             forgejo = \"alice-fj\"\n\
             gitlab = \"alice-gl\"\n\
             [people.bruno]\n\
             github = \"bruno-gh\"\n\
             forgejo = \"bruno-fj\"\n\
             bitbucket = \"bruno-bb\"\n",
        )
        .unwrap()
    }

    #[test]
    fn revocation_propagates_instead_of_union() {
        let read = |forge: Forge, handle: &str| {
            observed(forge, 200, vec![team_grant(handle, "read", None)])
        };
        let mut forges = BTreeMap::new();
        for forge in ["forgejo", "gitlab", "github"] {
            forges.insert(
                forge.into(),
                vec![StoredGrant {
                    person: "alice".into(),
                    org: "acme".into(),
                    team: Some("dev".into()),
                    repo: None,
                    level: Level::Read,
                    at: 100,
                }],
            );
        }
        let previous = Baseline { schema: 1, forges };
        // Forgejo dropped the grant; GitHub and GitLab still show it. A
        // union would keep it; the merge revokes it on GitLab while the
        // frozen GitHub copy is reported, never written.
        let with_current = vec![
            observed(Forge::Forgejo, 200, vec![]),
            read(Forge::Gitlab, "alice-gl"),
            read(Forge::Github, "alice-gh"),
        ];
        let report = plan(
            &maps_toml(),
            Some(&previous),
            &with_current,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].call.forge, "gitlab");
        assert_eq!(report.actions[0].call.operation, "revoke");
        assert_eq!(report.actions[0].call.method, "DELETE");
        assert_eq!(report.skipped_frozen.len(), 1);
        assert!(!report.complete);
    }

    #[test]
    fn level_change_mirrors_everywhere() {
        let mut forges = BTreeMap::new();
        for forge in ["github", "forgejo", "gitlab"] {
            forges.insert(
                forge.into(),
                vec![StoredGrant {
                    person: "alice".into(),
                    org: "acme".into(),
                    team: Some("dev".into()),
                    repo: None,
                    level: Level::Read,
                    at: 100,
                }],
            );
        }
        let previous = Baseline { schema: 1, forges };
        let files = vec![
            observed(
                Forge::Github,
                300,
                vec![team_grant("alice-gh", "read", None)],
            ),
            observed(
                Forge::Forgejo,
                300,
                vec![team_grant("alice-fj", "write", Some(200))],
            ),
            observed(
                Forge::Gitlab,
                300,
                vec![team_grant("alice-gl", "read", None)],
            ),
        ];
        let report = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert!(report.conflicts.is_empty());
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].call.forge, "gitlab");
        assert_eq!(report.actions[0].level, Some(Level::Write));
        // GitLab already carries the membership: an in-place update.
        assert_eq!(report.actions[0].call.method, "PUT");
        assert_eq!(report.skipped_frozen.len(), 1);
    }

    #[test]
    fn latest_change_wins_and_overwritten_is_reported() {
        let mut forges = BTreeMap::new();
        for forge in ["forgejo", "gitlab", "github"] {
            forges.insert(
                forge.into(),
                vec![StoredGrant {
                    person: "alice".into(),
                    org: "acme".into(),
                    team: Some("dev".into()),
                    repo: None,
                    level: Level::Read,
                    at: 100,
                }],
            );
        }
        let previous = Baseline { schema: 1, forges };
        let files = vec![
            observed(
                Forge::Forgejo,
                300,
                vec![team_grant("alice-fj", "write", Some(150))],
            ),
            observed(
                Forge::Gitlab,
                300,
                vec![team_grant("alice-gl", "admin", Some(250))],
            ),
        ];
        let report = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert_eq!(report.conflicts.len(), 1);
        assert!(report.conflicts[0].resolved);
        assert_eq!(report.conflicts[0].winner.as_ref().unwrap().forge, "gitlab");
        assert_eq!(
            report.conflicts[0].winner.as_ref().unwrap().level,
            Some(Level::Admin)
        );
        assert!(report.conflicts[0]
            .overwritten
            .iter()
            .any(|e| e.forge == "forgejo" && e.level == Some(Level::Write)));
        // GitLab already carries admin; only the frozen skip remains besides it.
        assert!(report.actions.iter().all(|a| a.call.forge != "github"));
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].call.forge, "forgejo");
    }

    #[test]
    fn tied_changes_pick_no_winner() {
        let mut forges = BTreeMap::new();
        for forge in ["forgejo", "gitlab"] {
            forges.insert(
                forge.into(),
                vec![StoredGrant {
                    person: "alice".into(),
                    org: "acme".into(),
                    team: Some("dev".into()),
                    repo: None,
                    level: Level::Read,
                    at: 100,
                }],
            );
        }
        let previous = Baseline { schema: 1, forges };
        let files = vec![
            observed(
                Forge::Forgejo,
                300,
                vec![team_grant("alice-fj", "write", Some(200))],
            ),
            observed(
                Forge::Gitlab,
                300,
                vec![team_grant("alice-gl", "admin", Some(200))],
            ),
        ];
        let report = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert!(report.actions.is_empty());
        assert_eq!(report.conflicts.len(), 1);
        assert!(!report.conflicts[0].resolved);
        assert!(report.conflicts[0].winner.is_none());
        assert!(!report.complete);
    }

    #[test]
    fn unmapped_accounts_are_reported_never_touched() {
        let files = vec![
            observed(
                Forge::Forgejo,
                100,
                vec![("mallory-fj", "acme", Some("dev"), None, "admin", None)],
            ),
            observed(Forge::Gitlab, 100, vec![]),
        ];
        let previous = Baseline {
            schema: 1,
            forges: BTreeMap::new(),
        };
        let report = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert_eq!(report.unmapped_accounts.len(), 1);
        assert_eq!(report.unmapped_accounts[0].handle, "mallory-fj");
        assert!(report.actions.is_empty());
        assert!(report.skipped_frozen.is_empty());
        assert!(report.complete);
    }

    #[test]
    fn personal_namespace_owners_are_never_removed() {
        let mut forges = BTreeMap::new();
        for forge in ["forgejo", "gitlab"] {
            forges.insert(
                forge.into(),
                vec![StoredGrant {
                    person: "alice".into(),
                    org: "person:alice".into(),
                    team: Some("dev".into()),
                    repo: None,
                    level: Level::Admin,
                    at: 100,
                }],
            );
        }
        let previous = Baseline { schema: 1, forges };
        // Forgejo lost the owner grant; GitLab still shows admin. The merge
        // wants a revoke on GitLab but the namespace owner is protected.
        let files = vec![
            observed(Forge::Forgejo, 200, vec![]),
            observed(
                Forge::Gitlab,
                200,
                vec![("alice-gl", "alice-fj", Some("dev"), None, "owner", None)],
            ),
        ];
        let report = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert!(report.actions.is_empty());
        assert!(report
            .skipped_protected
            .iter()
            .any(|s| s.call.forge == "gitlab" && s.reason == "owner-admin-protected"));
        // Refusals are steady state: no writable action remains, so the plan
        // counts as complete while still reporting the refusal.
        assert!(report.complete);
    }

    #[test]
    fn operator_access_is_never_touched() {
        let mut forges = BTreeMap::new();
        for forge in ["forgejo", "gitlab"] {
            forges.insert(
                forge.into(),
                vec![StoredGrant {
                    person: "alice".into(),
                    org: "acme".into(),
                    team: Some("dev".into()),
                    repo: None,
                    level: Level::Write,
                    at: 100,
                }],
            );
        }
        let previous = Baseline { schema: 1, forges };
        let files = vec![
            observed(Forge::Forgejo, 200, vec![]),
            observed(
                Forge::Gitlab,
                200,
                vec![team_grant("alice-gl", "write", None)],
            ),
        ];
        let mut guarded = options();
        guarded.operator = Some("alice".into());
        let report = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &guarded,
            &roles(),
            &grants(),
        )
        .unwrap();
        assert!(report.actions.is_empty());
        assert!(report
            .skipped_protected
            .iter()
            .any(|s| s.reason == "operator"));
        // The refusal is steady state: nothing writable remains.
        assert!(report.complete);
    }

    #[test]
    fn drift_reports_without_writes() {
        let mut forges = BTreeMap::new();
        forges.insert(
            "forgejo".into(),
            vec![StoredGrant {
                person: "alice".into(),
                org: "acme".into(),
                team: Some("dev".into()),
                repo: None,
                level: Level::Write,
                at: 100,
            }],
        );
        forges.insert(
            "gitlab".into(),
            vec![StoredGrant {
                person: "alice".into(),
                org: "acme".into(),
                team: Some("dev".into()),
                repo: None,
                level: Level::Read,
                at: 100,
            }],
        );
        let previous = Baseline { schema: 1, forges };
        // Current matches the divergent baseline exactly: no events, but the
        // two present levels disagree, so drift is reported and nothing runs.
        let files = vec![
            observed(
                Forge::Forgejo,
                200,
                vec![team_grant("alice-fj", "write", None)],
            ),
            observed(
                Forge::Gitlab,
                200,
                vec![team_grant("alice-gl", "read", None)],
            ),
        ];
        let report = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert!(report.actions.is_empty());
        assert_eq!(report.drift.len(), 1);
        assert!(!report.complete);
    }

    #[test]
    fn missing_baseline_plans_nothing() {
        let files = vec![
            observed(
                Forge::Forgejo,
                100,
                vec![team_grant("alice-fj", "write", Some(90))],
            ),
            observed(Forge::Gitlab, 100, vec![]),
        ];
        let report = plan(&maps_toml(), None, &files, &options(), &roles(), &grants()).unwrap();
        assert!(!report.baseline_present);
        assert!(report.actions.is_empty());
        assert!(!report.complete);
    }

    #[test]
    fn baseline_round_trips_through_worlds() {
        let maps = maps();
        let files = vec![
            observed(
                Forge::Forgejo,
                100,
                vec![team_grant("alice-fj", "write", Some(90))],
            ),
            observed(Forge::Gitlab, 100, vec![]),
        ];
        let (world, _, _, _) = observe(&maps, &files, &roles()).unwrap();
        let baseline = snapshot_world(&world);
        assert_eq!(baseline.schema, 1);
        let back = world_from_baseline(&maps, &baseline).unwrap();
        assert_eq!(back.forges.len(), world.forges.len());
        let report = plan(
            &maps_toml(),
            Some(&baseline),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert!(report.complete);
    }

    #[test]
    fn unwired_transport_refuses_every_write() {
        let plan = AccessPlan {
            baseline_present: true,
            complete: false,
            actions: vec![PlannedAction {
                person: "alice".into(),
                org: "acme".into(),
                team: Some("dev".into()),
                repo: None,
                source_forge: "forgejo".into(),
                event_time: 90,
                level: Some(Level::Write),
                call: Call {
                    forge: "gitlab".into(),
                    operation: "set-level".into(),
                    method: "PUT".into(),
                    path: "groups/acme/members".into(),
                    body: String::new(),
                    lookups: Vec::new(),
                    endpoint_verified: false,
                },
            }],
            skipped_frozen: Vec::new(),
            skipped_protected: Vec::new(),
            conflicts: Vec::new(),
            drift: Vec::new(),
            unmapped_accounts: Vec::new(),
            unmapped_targets: Vec::new(),
            notes: Vec::new(),
        };
        let maps = maps();
        let observed = BTreeSet::from(["gitlab".to_owned()]);
        let previous = Baseline {
            schema: 1,
            forges: BTreeMap::new(),
        };
        let mut transport = UnwiredTransport;
        let report = apply_plan(
            &plan,
            &mut transport,
            Duration::from_millis(1),
            &maps,
            &observed,
            &previous,
        );
        assert_eq!(report.attempted, 1);
        assert!(report.succeeded.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert!(report.stopped.is_none());
        assert!(!report.complete);
        assert!(report.next.forges.is_empty());
    }

    struct Script {
        calls: Vec<Call>,
        results: Vec<std::result::Result<(), ApiError>>,
    }

    impl Transport for Script {
        fn execute(&mut self, call: &Call) -> std::result::Result<(), ApiError> {
            self.calls.push(call.clone());
            if self.results.is_empty() {
                return Ok(());
            }
            self.results.remove(0)
        }
    }

    fn observed_names(forges: &[Forge]) -> BTreeSet<String> {
        forges
            .iter()
            .map(|forge| forge.as_str().to_owned())
            .collect()
    }

    fn files_from_world(validated: &ValidatedMaps, world: &World, at: i64) -> Vec<ObservedFile> {
        let mut files = Vec::new();
        for (forge, grants) in &world.forges {
            let mut raws = Vec::new();
            for (target, grant) in grants {
                let handle = validated
                    .handle_for(&target.person, forge.as_str())
                    .unwrap()
                    .to_owned();
                let org = match target.org.strip_prefix("person:") {
                    Some(owner) => validated
                        .handle_for(owner, forge.as_str())
                        .unwrap()
                        .to_owned(),
                    None => target.org.clone(),
                };
                let role = match grant.level {
                    Level::Read => "read",
                    Level::Write => "write",
                    Level::Admin => "admin",
                };
                raws.push(ObservedGrant {
                    handle,
                    org,
                    team: target.team.clone(),
                    repo: target.repo.clone(),
                    role: role.into(),
                    changed_at: None,
                });
            }
            files.push(ObservedFile {
                forge: *forge,
                observed_at: at,
                grants: raws,
            });
        }
        files
    }

    #[test]
    fn handles_match_case_insensitively() {
        let parsed: IdentityMap = toml::from_str(
            "schema = 1\n[people.alice]\nForgeJo = \"Alice-FJ\"\ngitlab = \"alice-gl\"\n",
        )
        .unwrap();
        let maps = parsed.maps().unwrap();
        assert_eq!(maps.person_for("forgejo", "alice-fj"), Some("alice"));
        assert_eq!(maps.person_for("forgejo", "ALICE-FJ"), Some("alice"));
        assert_eq!(maps.handle_for("alice", "forgejo"), Some("Alice-FJ"));
    }

    #[test]
    fn baseline_canonicalises_like_snapshots() {
        let previous = Baseline {
            schema: 1,
            forges: BTreeMap::from([(
                "forgejo".to_owned(),
                vec![StoredGrant {
                    person: "alice".into(),
                    org: "ACME".into(),
                    team: Some("DEV".into()),
                    repo: None,
                    level: Level::Read,
                    at: 100,
                }],
            )]),
        };
        let files = vec![observed(
            Forge::Forgejo,
            200,
            vec![team_grant("alice-fj", "read", None)],
        )];
        // Uppercase baseline spellings canonicalise to the same target:
        // no phantom event, a converged plan.
        let report = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert!(report.actions.is_empty());
        assert!(report.complete);
    }

    #[test]
    fn frozen_forge_with_converged_reality_reports_no_skip() {
        let mut forges = BTreeMap::new();
        for forge in ["github", "forgejo", "gitlab"] {
            forges.insert(
                forge.into(),
                vec![StoredGrant {
                    person: "alice".into(),
                    org: "acme".into(),
                    team: Some("dev".into()),
                    repo: None,
                    level: Level::Read,
                    at: 100,
                }],
            );
        }
        let previous = Baseline { schema: 1, forges };
        // GitHub was fixed directly to the winning level; only GitLab still
        // needs the mirror. The stale baseline entry alone must not report
        // a frozen skip for GitHub.
        let files = vec![
            observed(
                Forge::Github,
                300,
                vec![team_grant("alice-gh", "write", None)],
            ),
            observed(
                Forge::Forgejo,
                300,
                vec![team_grant("alice-fj", "write", Some(200))],
            ),
            observed(
                Forge::Gitlab,
                300,
                vec![team_grant("alice-gl", "read", None)],
            ),
        ];
        let report = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].call.forge, "gitlab");
        assert!(report.skipped_frozen.is_empty());
    }

    #[test]
    fn stale_baseline_entries_fail_closed() {
        // Alice holds no Bitbucket account, so a baseline claiming one is
        // stale after an identity edit and must be pruned explicitly.
        let previous = Baseline {
            schema: 1,
            forges: BTreeMap::from([(
                "bitbucket".to_owned(),
                vec![StoredGrant {
                    person: "alice".into(),
                    org: "acme".into(),
                    team: Some("dev".into()),
                    repo: None,
                    level: Level::Read,
                    at: 100,
                }],
            )]),
        };
        let files = vec![observed(Forge::Bitbucket, 200, vec![])];
        assert!(plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants()
        )
        .is_err());
    }

    #[test]
    fn apply_stops_on_first_auth_throttle_or_plan_limit_and_records_state() {
        let validated = maps_toml().maps().unwrap();
        let files = vec![
            observed(
                Forge::Github,
                100,
                vec![
                    team_grant("alice-gh", "write", Some(50)),
                    ("bruno-gh", "acme", None, Some("widget"), "write", Some(51)),
                ],
            ),
            observed(
                Forge::Forgejo,
                100,
                vec![
                    team_grant("alice-fj", "write", Some(52)),
                    ("bruno-fj", "acme", None, Some("widget"), "write", Some(53)),
                ],
            ),
            observed(Forge::Gitlab, 100, vec![]),
            observed(Forge::Bitbucket, 100, vec![]),
        ];
        let previous = Baseline {
            schema: 1,
            forges: BTreeMap::new(),
        };
        let planned = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert_eq!(planned.actions.len(), 2);
        for status in [401, 402, 403, 429] {
            let mut script = Script {
                calls: Vec::new(),
                results: vec![Err(ApiError::status(status, "halt"))],
            };
            let report = apply_plan(
                &planned,
                &mut script,
                Duration::ZERO,
                &validated,
                &observed_names(&[
                    Forge::Github,
                    Forge::Forgejo,
                    Forge::Gitlab,
                    Forge::Bitbucket,
                ]),
                &previous,
            );
            // Exactly one transport call, then the run stops: no further
            // calls, no baseline advancement for failed or skipped actions.
            assert_eq!(report.attempted, 1, "{status}");
            assert_eq!(script.calls.len(), 1, "{status}");
            assert!(report.stopped.is_some(), "{status}");
            // The unattempted call is recorded too, so its key never advances.
            assert_eq!(report.failed.len(), 2, "{status}");
            assert!(!report.complete, "{status}");
            assert!(report.next.forges.values().all(Vec::is_empty), "{status}");
        }
    }

    #[test]
    fn apply_skips_failed_forge_and_advances_the_rest() {
        let validated = maps_toml().maps().unwrap();
        let files = vec![
            observed(
                Forge::Github,
                100,
                vec![
                    team_grant("alice-gh", "write", Some(50)),
                    ("bruno-gh", "acme", None, Some("widget"), "write", Some(51)),
                ],
            ),
            observed(
                Forge::Forgejo,
                100,
                vec![team_grant("alice-fj", "write", Some(52))],
            ),
            observed(Forge::Gitlab, 100, vec![]),
            observed(Forge::Bitbucket, 100, vec![]),
        ];
        let previous = Baseline {
            schema: 1,
            forges: BTreeMap::new(),
        };
        let planned = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert_eq!(planned.actions.len(), 3);
        let mut script = Script {
            calls: Vec::new(),
            results: vec![Ok(()), Err(ApiError::status(500, "exploded"))],
        };
        let report = apply_plan(
            &planned,
            &mut script,
            Duration::ZERO,
            &validated,
            &observed_names(&[
                Forge::Github,
                Forge::Forgejo,
                Forge::Gitlab,
                Forge::Bitbucket,
            ]),
            &previous,
        );
        assert_eq!(report.attempted, 3);
        assert!(report.stopped.is_none());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].status, Some(500));
        assert!(!report.complete);
        // Alice's key mirrored fully: baseline advances on every mapped
        // observed forge. Bruno's key lost its Forgejo write: nothing
        // advances, preserving the event for the next run.
        let levels = |forge: &str, person: &str| {
            report.next.forges.get(forge).map_or(Vec::new(), |grants| {
                grants
                    .iter()
                    .filter(|grant| grant.person == person)
                    .map(|grant| grant.level)
                    .collect::<Vec<_>>()
            })
        };
        for forge in ["github", "forgejo", "gitlab"] {
            assert_eq!(levels(forge, "alice"), vec![Level::Write], "{forge}");
        }
        assert!(report
            .next
            .forges
            .values()
            .all(|grants| grants.iter().all(|grant| grant.person != "bruno")));
    }

    #[test]
    fn successful_apply_converges() {
        let validated = maps_toml().maps().unwrap();
        let files = vec![
            observed(
                Forge::Github,
                100,
                vec![team_grant("alice-gh", "write", Some(50))],
            ),
            observed(Forge::Forgejo, 100, vec![]),
            observed(Forge::Gitlab, 100, vec![]),
        ];
        let previous = Baseline {
            schema: 1,
            forges: BTreeMap::new(),
        };
        let planned = plan(
            &maps_toml(),
            Some(&previous),
            &files,
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert_eq!(planned.actions.len(), 2);
        let mut script = Script {
            calls: Vec::new(),
            results: Vec::new(),
        };
        let report = apply_plan(
            &planned,
            &mut script,
            Duration::ZERO,
            &validated,
            &observed_names(&[Forge::Github, Forge::Forgejo, Forge::Gitlab]),
            &previous,
        );
        assert!(report.complete);
        assert_eq!(script.calls.len(), 2);
        // Re-planning from the advanced baseline over identical reality is
        // silent: applying is idempotent.
        let world = world_from_baseline(&validated, &report.next).unwrap();
        let again = plan(
            &maps_toml(),
            Some(&report.next),
            &files_from_world(&validated, &world, 400),
            &options(),
            &roles(),
            &grants(),
        )
        .unwrap();
        assert!(again.actions.is_empty());
        assert!(again.complete);
    }
}
