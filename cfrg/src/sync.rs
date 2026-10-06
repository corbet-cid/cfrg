//! Explicit, fast-forward-only reconciliation of declared Git locations.
use crate::{failure, placement::Policy, profile, Result};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

#[path = "sync_content.rs"]
mod content;
#[path = "sync_git.rs"]
mod git;
pub use content::Content;
use git::{supported_ref, Git};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Current,
    Planned,
    Updated,
    Blocked,
    Pending,
}

#[derive(Debug, Serialize)]
pub struct Ref {
    pub name: String,
    pub source_oid: String,
    pub observed_oid: Option<String>,
    pub state: State,
    pub reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Replica {
    pub forge: String,
    pub state: State,
    pub reason: &'static str,
    pub refs: Vec<Ref>,
    /// Never deleted. Under --all-refs these require an explicit policy decision.
    pub retained_refs: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub repository: String,
    pub source_forge: String,
    pub all_refs: bool,
    pub apply: bool,
    pub source_refs: BTreeMap<String, String>,
    pub source_state: State,
    pub source_reason: &'static str,
    pub content: Content,
    pub replicas: Vec<Replica>,
    /// Every selected ref at every selected replica was verified at the source OID.
    pub complete: bool,
    /// Also requires all policy secondaries, all heads/tags and no external payloads.
    pub repository_complete: bool,
}

impl Report {
    pub fn completed(&self) -> bool {
        self.complete
    }
}

/// Inspect first; write only when `apply` is explicit. No creation, deletion,
/// pruning, force, primary promotion or provider APIs are performed.
pub fn reconcile(
    policy: &Policy,
    repository: &str,
    refs: &[String],
    all_refs: bool,
    destinations: &[String],
    apply: bool,
    timeout: Duration,
) -> Result<Report> {
    let plan = policy.plan(repository)?;
    let repo = &policy.repositories[repository];
    if timeout.is_zero() || (all_refs != refs.is_empty()) || destinations.is_empty() {
        return Err(failure(
            "Choose explicit refs or all-refs, explicit destinations and a positive timeout",
        ));
    }
    let unique_refs: BTreeSet<_> = refs.iter().collect();
    let unique_destinations: BTreeSet<_> = destinations.iter().collect();
    if unique_refs.len() != refs.len() || unique_destinations.len() != destinations.len() {
        return Err(failure(
            "Reconciliation refs and destinations must be unique",
        ));
    }
    for target in destinations {
        if target == &plan.primary_forge || !repo.locations.contains_key(target) {
            return Err(failure(
                "Destination must be an explicitly declared secondary forge",
            ));
        }
    }
    if profile::is_profile_repo(repository)
        || repo
            .locations
            .values()
            .any(|path| profile::is_profile_repo(path))
    {
        return Err(failure(
            "Profile repositories are projected, not mirrored 1:1; project their content instead",
        ));
    }
    // Use the clone endpoint directly; following redirects could move a push
    // outside the declared secondary (including back to the primary).
    let url = |forge: &str| format!("{}/{}.git", policy.forges[forge].url, repo.locations[forge]);
    let source_url = url(&plan.primary_forge);
    let directory = tempfile::Builder::new().prefix("cfrg-sync-").tempdir()?;
    let git = Git::new(directory.path(), timeout)?;
    for name in refs {
        if !supported_ref(name) {
            return Err(failure("Only exact refs/heads and refs/tags are supported"));
        }
        git.run(&["check-ref-format", name])?;
    }
    let mut report = Report {
        repository: repository.into(),
        source_forge: plan.primary_forge.clone(),
        all_refs,
        apply,
        source_refs: BTreeMap::new(),
        source_state: State::Pending,
        source_reason: "source-unavailable",
        content: Content::default(),
        replicas: destinations
            .iter()
            .map(|forge| Replica {
                forge: forge.clone(),
                state: State::Pending,
                reason: "source-unavailable",
                refs: Vec::new(),
                retained_refs: Vec::new(),
            })
            .collect(),
        complete: false,
        repository_complete: false,
    };
    let Ok(source) = git.inventory(&source_url, refs, all_refs) else {
        return Ok(report);
    };
    report.source_refs = source;
    if refs
        .iter()
        .any(|name| !report.source_refs.contains_key(name))
    {
        report.source_state = State::Blocked;
        report.source_reason = "source-ref-missing";
        for replica in &mut report.replicas {
            replica.state = State::Blocked;
            replica.reason = "source-ref-missing";
        }
        return Ok(report);
    }
    for name in report.source_refs.keys() {
        git.run(&["check-ref-format", name])?;
    }
    let oid_len = report.source_refs.values().next().map_or(40, String::len);
    if report.source_refs.values().any(|oid| oid.len() != oid_len) {
        return Err(failure("Mixed Git object formats"));
    }
    git.run(&[
        "init",
        "--bare",
        "--quiet",
        "--template=",
        if oid_len == 64 {
            "--object-format=sha256"
        } else {
            "--object-format=sha1"
        },
    ])?;
    if git
        .fetch(&source_url, &report.source_refs, "source")
        .is_err()
    {
        return Ok(report);
    }
    for (name, oid) in &report.source_refs {
        if name.starts_with("refs/heads/") && git.run(&["cat-file", "-t", oid])? != "commit" {
            return Err(failure("A source branch must point directly to a commit"));
        }
    }
    report.source_state = State::Current;
    report.source_reason = "observed-source-fetched";
    let Ok(content) = content::inspect(&git, oid_len / 2) else {
        report.source_state = State::Pending;
        report.source_reason = "source-content-inspection-failed";
        for replica in &mut report.replicas {
            replica.reason = report.source_reason;
        }
        return Ok(report);
    };
    report.content = content;
    if !report.content.verified() {
        report.source_state = State::Blocked;
        report.source_reason = "external-content-unverified";
        for replica in &mut report.replicas {
            replica.state = State::Blocked;
            replica.reason = report.source_reason;
        }
        return Ok(report);
    }
    for (index, replica) in report.replicas.iter_mut().enumerate() {
        inspect_replica(
            &git,
            &url(&replica.forge),
            &report.source_refs,
            all_refs,
            index,
            replica,
        )?;
    }
    if apply {
        for replica in &mut report.replicas {
            if replica.state == State::Planned {
                apply_replica(&git, &url(&replica.forge), &report.source_refs, replica);
            }
        }
    }
    report.complete = report
        .replicas
        .iter()
        .all(|r| matches!(r.state, State::Current | State::Updated));
    match git.inventory(&source_url, refs, all_refs) {
        Ok(observed) if observed == report.source_refs => {}
        Ok(_) => {
            report.source_state = State::Pending;
            report.source_reason = "source-moved";
            report.complete = false;
        }
        Err(_) => {
            report.source_state = State::Pending;
            report.source_reason = "source-verification-unavailable";
            report.complete = false;
        }
    }
    report.repository_complete =
        report.complete && all_refs && destinations.len() + 1 == repo.locations.len();
    Ok(report)
}

/// How the observed replica ref relates to the source ref, once histories
/// are available. Pure input to [`plan_ref`]; the caller fetches history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ancestry {
    /// The replica commit is an ancestor of the source: fast-forward applies.
    Ancestor,
    /// The source commit is an ancestor of the replica: the replica is ahead.
    Descendant,
    /// Neither contains the other: the branch diverged.
    Diverged,
    /// History is unavailable: inspection cannot decide yet.
    Unknown,
}

/// Pure per-ref reconciliation plan: the replica state and reason for one
/// source ref and its observed replica ref. Tag conflicts always block;
/// branches advance only on a strict fast-forward.
pub fn plan_ref(
    name: &str,
    source_oid: &str,
    observed_oid: Option<&str>,
    ancestry: Ancestry,
) -> (State, &'static str) {
    let observed = match observed_oid {
        None => return (State::Planned, "create-ref"),
        Some(observed) => observed,
    };
    if observed == source_oid {
        return (State::Current, "ref-matches");
    }
    if name.starts_with("refs/tags/") {
        return (State::Blocked, "tag-conflict");
    }
    match ancestry {
        Ancestry::Ancestor => (State::Planned, "fast-forward"),
        Ancestry::Descendant => (State::Blocked, "replica-ahead"),
        Ancestry::Diverged => (State::Blocked, "diverged-branch"),
        Ancestry::Unknown => (State::Pending, "replica-history-unavailable"),
    }
}

fn inspect_replica(
    git: &Git,
    url: &str,
    source: &BTreeMap<String, String>,
    all: bool,
    index: usize,
    replica: &mut Replica,
) -> Result<()> {
    let selected: Vec<_> = source.keys().cloned().collect();
    let target = match git.inventory(url, &selected, all) {
        Ok(target) => target,
        Err(_) => {
            replica.reason = "replica-unavailable";
            return Ok(());
        }
    };
    replica.retained_refs = target
        .keys()
        .filter(|name| !source.contains_key(*name))
        .cloned()
        .collect();
    replica.state = State::Current;
    replica.reason = "refs-match";
    for (name, source_oid) in source {
        let observed_oid = target.get(name).cloned();
        // Ancestry decides branches only; absent refs, matches and tag
        // conflicts resolve through the same pure table without history.
        let ancestry = match observed_oid.as_deref() {
            Some(oid) if oid != source_oid && !name.starts_with("refs/tags/") => {
                if git
                    .run(&["cat-file", "-e", &format!("{oid}^{{commit}}")])
                    .is_err()
                    && git
                        .fetch(
                            url,
                            &BTreeMap::from([(name.clone(), oid.into())]),
                            &format!("target-{index}-{}", replica.refs.len()),
                        )
                        .is_err()
                {
                    Ancestry::Unknown
                } else {
                    match git.run(&["merge-base", oid, source_oid]) {
                        Ok(base) if base == oid => Ancestry::Ancestor,
                        Ok(base) if base == *source_oid => Ancestry::Descendant,
                        _ => Ancestry::Diverged,
                    }
                }
            }
            _ => Ancestry::Unknown,
        };
        let (state, reason) = plan_ref(name, source_oid, observed_oid.as_deref(), ancestry);
        replica.refs.push(Ref {
            name: name.clone(),
            source_oid: source_oid.clone(),
            observed_oid,
            state,
            reason,
        });
    }
    if replica.refs.iter().any(|r| r.state == State::Blocked) || !replica.retained_refs.is_empty() {
        replica.state = State::Blocked;
        replica.reason = "conflicting-or-replica-only-refs";
    } else if replica.refs.iter().any(|r| r.state == State::Pending) {
        replica.state = State::Pending;
        replica.reason = "replica-history-unavailable";
    } else if replica.refs.iter().any(|r| r.state == State::Planned) {
        replica.state = State::Planned;
        replica.reason = "safe-ref-updates";
    }
    Ok(())
}

fn apply_replica(git: &Git, url: &str, source: &BTreeMap<String, String>, replica: &mut Replica) {
    let mut args = vec![
        "push".into(),
        "--atomic".into(),
        "--porcelain".into(),
        "--no-follow-tags".into(),
        "--no-verify".into(),
        "--".into(),
        url.into(),
    ];
    for item in &replica.refs {
        if item.state == State::Planned {
            args.push(format!("{}:{}", item.source_oid, item.name));
        }
    }
    // One push only. A lost response is reconciled by reading, never replayed.
    let pushed = git.run_owned(&args).is_ok();
    let selected = source.keys().cloned().collect::<Vec<_>>();
    match git.inventory(url, &selected, false) {
        Ok(observed) if observed == *source => {
            replica.state = State::Updated;
            replica.reason = "verified-after-push";
            for item in &mut replica.refs {
                if item.state == State::Planned {
                    item.state = State::Updated;
                    item.reason = "verified-after-push";
                }
            }
        }
        _ => {
            replica.state = State::Pending;
            replica.reason = if pushed {
                "post-push-verification-unavailable"
            } else {
                "push-not-confirmed"
            };
            for item in &mut replica.refs {
                if item.state == State::Planned {
                    item.state = State::Pending;
                    item.reason = replica.reason;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_plans_cover_create_match_tags_and_ancestry() {
        assert_eq!(
            plan_ref("refs/heads/main", "a", None, Ancestry::Unknown),
            (State::Planned, "create-ref")
        );
        assert_eq!(
            plan_ref("refs/heads/main", "a", Some("a"), Ancestry::Diverged),
            (State::Current, "ref-matches")
        );
        assert_eq!(
            plan_ref("refs/tags/v1", "a", Some("b"), Ancestry::Ancestor),
            (State::Blocked, "tag-conflict")
        );
        assert_eq!(
            plan_ref("refs/heads/main", "a", Some("b"), Ancestry::Ancestor),
            (State::Planned, "fast-forward")
        );
        assert_eq!(
            plan_ref("refs/heads/main", "a", Some("b"), Ancestry::Descendant),
            (State::Blocked, "replica-ahead")
        );
        assert_eq!(
            plan_ref("refs/heads/main", "a", Some("b"), Ancestry::Diverged),
            (State::Blocked, "diverged-branch")
        );
        assert_eq!(
            plan_ref("refs/heads/main", "a", Some("b"), Ancestry::Unknown),
            (State::Pending, "replica-history-unavailable")
        );
    }

    #[test]
    fn profile_repositories_refuse_one_to_one_mirror() {
        let policy: Policy = serde_json::from_str(
            r#"{
                "schema": 1, "free_only": true,
                "forges": {
                    "hub": {"kind": "github", "url": "https://hub.example.org"},
                    "mirror": {"kind": "forgejo", "url": "https://forge.example.org"}
                },
                "ci": {
                    "primary": {"driver": "github-actions", "forge": "hub", "execution": "owned", "capabilities": ["linux-x86_64"]}
                },
                "repositories": {
                    ".github": {
                        "ci": "primary", "visibility": "public", "sensitive": false,
                        "locations": {"hub": "team/.github", "mirror": "backup/.github"},
                        "clone_fallbacks": ["mirror"], "promotion": "manual"
                    }
                }
            }"#,
        )
        .unwrap();
        let error = reconcile(
            &policy,
            ".github",
            &["refs/heads/main".into()],
            false,
            &["mirror".into()],
            false,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("projected"),
            "unexpected error: {error}"
        );
    }
}
