//! Declarative repository placement. Pure policy; no provider API or scheduler.
use crate::{canonical_repository, failure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema: u32,
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
    pub driver: String,
    pub forge: String,
    pub execution: Execution,
    /// Explicit coverage capabilities, including native OS/architecture where needed.
    pub capabilities: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Execution {
    Owned,
    FreeHosted,
    Paid,
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
    #[serde(default)]
    pub clone_fallbacks: Vec<String>,
    #[serde(default)]
    pub execution_fallbacks: Vec<String>,
    pub promotion: Promotion,
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
    pub execution_fallbacks: Vec<String>,
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
            let forge = self.forges.get(&ci.forge).ok_or_else(|| {
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
            let native_forge = match ci.driver.as_str() {
                "github-actions" => Some("github"),
                "gitlab-ci" => Some("gitlab"),
                "bitbucket-pipelines" => Some("bitbucket"),
                _ => None,
            };
            if native_forge.is_some_and(|kind| forge.kind != kind) {
                return Err(failure(format!("CI {id} must use its native forge")));
            }
            if self.free_only && ci.execution == Execution::Paid {
                return Err(failure(format!("CI {id} violates free_only")));
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
            let mut clones = BTreeSet::from([primary.forge.as_str()]);
            for forge in &repo.clone_fallbacks {
                if !clones.insert(forge) || !repo.locations.contains_key(forge) {
                    return Err(failure(format!(
                        "Repository {id} clone fallback must name each declared secondary once"
                    )));
                }
            }
            let mut executors = BTreeSet::new();
            for executor in std::iter::once(&repo.ci).chain(&repo.execution_fallbacks) {
                let ci = self.ci.get(executor).ok_or_else(|| {
                    failure(format!(
                        "Repository {id} has an undeclared execution fallback"
                    ))
                })?;
                if !executors.insert(executor) || !repo.locations.contains_key(&ci.forge) {
                    return Err(failure(format!(
                        "Repository {id} execution locations must be declared and unique"
                    )));
                }
                if ci.execution == Execution::FreeHosted
                    && (repo.visibility != cqlt::Visibility::Public || repo.sensitive)
                {
                    return Err(failure(format!(
                        "Repository {id} is not eligible for public free hosted execution"
                    )));
                }
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
            execution_fallbacks: repo.execution_fallbacks.clone(),
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

/// No API calls or inferred availability. Unknown and active submissions block
/// failover; check failures never turn into an excuse to try another provider.
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
    let order: Vec<_> = std::iter::once(&repo.ci)
        .chain(&repo.execution_fallbacks)
        .collect();
    if required.is_empty() || states.keys().any(|ci| !order.contains(&ci)) {
        return Err(failure(
            "Execution decision requires explicit coverage and only declared providers",
        ));
    }
    // Reconcile ALL providers before dispatch: an old fallback may still own it.
    for ci in &order {
        match states.get(*ci) {
            None | Some(RunState::Unknown) => {
                return Ok(Decision::Blocked {
                    reason: format!("Unresolved provider {ci}"),
                })
            }
            Some(RunState::Active) => return Ok(Decision::Attach { ci: (*ci).clone() }),
            Some(RunState::Failed) => return Ok(Decision::Failed { ci: (*ci).clone() }),
            _ => (),
        }
    }
    for ci in &order {
        if matches!(states.get(*ci), Some(RunState::Succeeded))
            && required
                .iter()
                .all(|c| policy.ci[*ci].capabilities.contains(c))
        {
            return Ok(Decision::Reuse { ci: (*ci).clone() });
        }
    }
    for ci in &order {
        if matches!(states.get(*ci), Some(RunState::Absent))
            && required
                .iter()
                .all(|c| policy.ci[*ci].capabilities.contains(c))
        {
            return Ok(Decision::Dispatch { ci: (*ci).clone() });
        }
    }
    Ok(Decision::Blocked {
        reason: "No available provider covers this request".into(),
    })
}
