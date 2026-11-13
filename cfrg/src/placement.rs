//! Declarative repository placement. Pure policy; no provider API or scheduler.
use crate::{canonical_repository, failure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema: u32,
    /// Accepted so existing documents parse; it has no effect. Every executor is owned (below), so there is nothing paid or
    /// provider-hosted for it to forbid.
    #[serde(default)]
    pub free_only: bool,
    pub forges: BTreeMap<String, Forge>,
    pub ci: BTreeMap<String, Ci>,
    pub repositories: BTreeMap<String, Repository>,
    #[serde(default)]
    pub placement_rules: Vec<PlacementRule>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Forge {
    /// Provider names are descriptive; Git transport is common to every forge.
    pub kind: String,
    /// Canonical web/HTTPS clone base, including an optional installation prefix.
    pub url: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ci {
    /// The engine. The own CI of a mirror forge (`github-actions`, `gitlab-ci`, `bitbucket-pipelines`) is refused: no CI runs on
    /// a mirror (operating model R6, R7).
    pub driver: String,
    pub forge: String,
    pub execution: Execution,
    /// Explicit coverage capabilities, including native OS/architecture where needed.
    pub capabilities: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Execution {
    /// Our own compute, the only kind there is: a provider-hosted (`free-hosted`) or `paid` executor cannot be modelled.
    Owned,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub ci: String,
    pub visibility: cqlt::Visibility,
    pub sensitive: bool,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    /// Forge declaration -> complete namespace/repository path; nested groups allowed.
    pub locations: BTreeMap<String, String>,
    /// Forge declaration -> external content handling at that location. Absent
    /// (or `block`) refuses every repository whose history contains LFS pointers
    /// or submodule gitlinks; `pointers-only` explicitly permits promoting refs
    /// and those pointer blobs while the payloads and submodule commits stay at
    /// the primary. Never declared for the primary itself.
    #[serde(default)]
    pub external_content: BTreeMap<String, ExternalContent>,
    #[serde(default)]
    pub clone_fallbacks: Vec<String>,
    /// Removed capability: a CI executor on a mirror forge cannot be modelled (operating model R6, R7). The key is still
    /// accepted EMPTY, because deployed documents carry `[]`; any executor in it is refused by `validate`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub execution_fallbacks: Vec<String>,
    pub promotion: Promotion,
}

/// How one location treats external content (LFS payloads, submodule commits)
/// named by the source history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExternalContent {
    /// Refuse promotion; the payloads cannot be verified at this location (default).
    Block,
    /// Promote refs and pointer blobs only; external payloads stay at the primary.
    PointersOnly,
}

impl Repository {
    /// Whether this location is explicitly allowed a pointer-only mirror.
    pub fn pointers_only(&self, forge: &str) -> bool {
        self.external_content.get(forge) == Some(&ExternalContent::PointersOnly)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Promotion {
    Manual,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementRule {
    pub name: String,
    /// All attributes must match. Rules constrain explicit placements, never create repos.
    pub attributes: BTreeMap<String, String>,
    pub require: Vec<String>,
    pub forbid: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub repository: String,
    pub primary_ci: String,
    pub primary_forge: String,
    pub trigger_forge: String,
    pub clone_urls: Vec<String>,
    pub promotion: &'static str,
}

fn plain(value: &str) -> bool {
    cqlt::valid_login(value)
}

impl Policy {
    pub fn validate(&self) -> Result<()> {
        if self.schema != 1 {
            return Err(failure("Unsupported repository policy schema"));
        }
        for (id, forge) in &self.forges {
            if !plain(id)
                || !plain(&forge.kind)
                || !forge.url.starts_with("https://")
                || forge.url.ends_with('/')
            {
                return Err(failure(format!("Forge {id} requires a name, kind and canonical HTTPS base without a trailing slash")));
            }
            canonical_repository(&format!("{}/fixture/repo", forge.url))?;
        }
        for (id, ci) in &self.ci {
            self.forges.get(&ci.forge).ok_or_else(|| {
                failure(format!("CI {id} refers to undeclared forge {}", ci.forge))
            })?;
            if !plain(id)
                || !plain(&ci.driver)
                || ci.capabilities.is_empty()
                || ci.capabilities.iter().any(|c| !plain(c))
            {
                return Err(failure(format!(
                    "CI {id} requires an explicit driver and nonempty coverage capabilities"
                )));
            }
            if ["github-actions", "gitlab-ci", "bitbucket-pipelines"].contains(&ci.driver.as_str())
            {
                return Err(failure(format!(
                    "CI {id} is the own CI of a mirror forge ({}), which cannot be modelled (operating model R6, R7)",
                    ci.driver
                )));
            }
        }
        for rule in &self.placement_rules {
            if rule.name.is_empty() || rule.attributes.is_empty() {
                return Err(failure(
                    "Placement rules require a name and nonempty attribute match",
                ));
            }
            for forge in rule.require.iter().chain(&rule.forbid) {
                if !self.forges.contains_key(forge) {
                    return Err(failure("Placement rule names an undeclared forge"));
                }
            }
            if rule.require.iter().any(|f| rule.forbid.contains(f)) {
                return Err(failure("Placement rule both requires and forbids a forge"));
            }
        }
        let mut identities = BTreeMap::new();
        for (id, repo) in &self.repositories {
            if !plain(id) {
                return Err(failure("Invalid logical repository ID"));
            }
            let primary = self.ci.get(&repo.ci).ok_or_else(|| {
                failure(format!("Repository {id} refers to undeclared primary CI"))
            })?;
            if !repo.locations.contains_key(&primary.forge) {
                return Err(failure(format!(
                    "Repository {id} must live on primary CI's forge {}",
                    primary.forge
                )));
            }
            for (forge, path) in &repo.locations {
                if path.split('/').count() < 2
                    || path.split('/').any(|part| !plain(part))
                    || path.ends_with(".git")
                {
                    return Err(failure(format!(
                        "Repository {id} requires a canonical namespace/repository path"
                    )));
                }
                let url = self.location(repo, forge)?;
                let identity = canonical_repository(&url)?;
                if let Some(other) = identities.insert(identity, id) {
                    return Err(failure(format!(
                        "Repositories {other} and {id} claim the same remote location"
                    )));
                }
            }
            for forge in repo.external_content.keys() {
                if !repo.locations.contains_key(forge) {
                    return Err(failure(format!(
                        "Repository {id} declares external content handling for an undeclared location {forge}"
                    )));
                }
                if forge == &primary.forge {
                    return Err(failure(format!(
                        "Repository {id} cannot declare external content handling at its primary forge"
                    )));
                }
            }
            let mut clones = BTreeSet::from([primary.forge.as_str()]);
            for forge in &repo.clone_fallbacks {
                if !clones.insert(forge) || !repo.locations.contains_key(forge) {
                    return Err(failure(format!(
                        "Repository {id} clone fallback must name each declared secondary once"
                    )));
                }
            }
            // The primary is the only executor: one on a mirror forge cannot be modelled (operating model R6, R7).
            if !repo.execution_fallbacks.is_empty() {
                return Err(failure(format!(
                    "Repository {id} names a CI executor on a mirror forge (execution_fallbacks), which cannot be modelled (operating model R6, R7)"
                )));
            }
            for rule in &self.placement_rules {
                if rule
                    .attributes
                    .iter()
                    .all(|(k, v)| repo.attributes.get(k) == Some(v))
                    && (rule.require.iter().any(|f| !repo.locations.contains_key(f))
                        || rule.forbid.iter().any(|f| repo.locations.contains_key(f)))
                {
                    return Err(failure(format!(
                        "Repository {id} violates placement rule {}",
                        rule.name
                    )));
                }
            }
        }
        Ok(())
    }

    fn location(&self, repo: &Repository, forge: &str) -> Result<String> {
        let base = self
            .forges
            .get(forge)
            .ok_or_else(|| failure("Undeclared forge"))?;
        let path = repo
            .locations
            .get(forge)
            .ok_or_else(|| failure("Undeclared repository location"))?;
        Ok(format!("{}/{path}", base.url))
    }

    pub fn plan(&self, repository: &str) -> Result<Plan> {
        self.validate()?;
        let repo = self
            .repositories
            .get(repository)
            .ok_or_else(|| failure("Undeclared repository"))?;
        let primary = &self.ci[&repo.ci].forge;
        let clone_urls = std::iter::once(primary)
            .chain(&repo.clone_fallbacks)
            .map(|forge| self.location(repo, forge))
            .collect::<Result<_>>()?;
        Ok(Plan {
            repository: repository.into(),
            primary_ci: repo.ci.clone(),
            primary_forge: primary.clone(),
            trigger_forge: primary.clone(),
            clone_urls,
            promotion: "manual",
        })
    }
}

/// Execution state observed by a provider adapter for ONE exact request
/// (source, dependency lock, tool/config/runtime identity and coverage).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunState {
    Absent,
    Unavailable,
    CancelledBeforeStart,
    Active,
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Decision {
    Dispatch { ci: String },
    Attach { ci: String },
    Reuse { ci: String },
    Failed { ci: String },
    Blocked { reason: String },
}

/// No API calls or inferred availability. An unknown or active submission blocks a new one; a failed check never turns
/// into an excuse to try another provider (there is none: the primary is the only executor).
pub fn execution_decision(
    policy: &Policy,
    repository: &str,
    required: &[String],
    states: &BTreeMap<String, RunState>,
) -> Result<Decision> {
    policy.validate()?;
    let repo = policy
        .repositories
        .get(repository)
        .ok_or_else(|| failure("Undeclared repository"))?;
    let ci = &repo.ci;
    if required.is_empty() || states.keys().any(|state| state != ci) {
        return Err(failure(
            "Execution decision requires explicit coverage and only the declared provider",
        ));
    }
    let covers = required
        .iter()
        .all(|c| policy.ci[ci].capabilities.contains(c));
    Ok(match states.get(ci) {
        None | Some(RunState::Unknown) => Decision::Blocked {
            reason: format!("Unresolved provider {ci}"),
        },
        Some(RunState::Active) => Decision::Attach { ci: ci.clone() },
        Some(RunState::Failed) => Decision::Failed { ci: ci.clone() },
        Some(RunState::Succeeded) if covers => Decision::Reuse { ci: ci.clone() },
        Some(RunState::Absent) if covers => Decision::Dispatch { ci: ci.clone() },
        _ => Decision::Blocked {
            reason: "No available provider covers this request".into(),
        },
    })
}
