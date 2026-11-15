//! Derived placement: the default copies of every repository on the forge, computed from the repository's own
//! properties instead of written down one by one.
//!
//! The placement's `repositories` list keeps only real exceptions (a hold, an override, a pinned path). A repository
//! the forge lists that is neither declared nor a contributed project (operating model R10, R11) gets a row derived
//! from the `derive` policy of the placement:
//!
//! | Property of the repository | Derived |
//! |---|---|
//! | owner (organisation or personal login) | the GitLab namespace and Bitbucket project of `derive.owners[owner]` |
//! | visibility | `private` of the row; the settings pass (R42) makes every copy match it |
//! | content | `lfs` when `.gitattributes` routes files through LFS (the copies then carry pointers only, R4), `profile` for `.github`, else `native-git` |
//! | declared (by path or source id) | nothing: the declared row overrides the derivation completely, even with no destinations |
//! | contributed, fork, pull mirror, empty | no row |
//!
//! No CI is derived for a copy (R6, R7): a destination has no CI field and the reconcile switches it off. GitHub is
//! never derived: a GitHub copy is a paced, gate-managed exception that stays declared.
//!
//! An owner with no mapping for a provider is a HOLD, never an error: it is returned once per owner and provider with
//! all affected repositories, for the daily report. An owner that deliberately has no copy there says `false`.
use super::{
    env_name, failure, path as check_path, Content, Destination, Endpoint, Placement, Provider,
    Repository,
};
use crate::{contributed::Contributed, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The `derive` policy of a placement.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Derive {
    /// Everything of a GitLab destination except its namespace. Absent: GitLab is never derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gitlab: Option<Template>,
    /// Everything of a Bitbucket destination except its project. Absent: Bitbucket is never derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitbucket: Option<Template>,
    /// Lowercase owner login to its mappings.
    #[serde(default)]
    pub owners: BTreeMap<String, OwnerMap>,
    /// When set, the Bitbucket destination of an LFS repository carries this hold (the reason): Bitbucket's HTTPS LFS
    /// endpoint refuses the API token, so such a copy waits for the SSH push mirror.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitbucket_lfs_hold: Option<String>,
    /// When true the Bitbucket destination of an LFS repository is an SSH push mirror (`use_ssh`): Forgejo then pushes git history
    /// only, so the copy carries pointers and no hold is needed. Takes precedence over `bitbucket_lfs_hold`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bitbucket_lfs_ssh: bool,
    /// Providers a `.github` (profile) repository is copied to. Absent: every derived provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_providers: Option<Vec<Provider>>,
    /// Throwaway and scratch repositories get no copy: lowercase `owner/repo` patterns where `*` matches any run of
    /// characters (`*/delete-me-*`, `ds-*/*`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub endpoint: Endpoint,
    pub mirror_user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restriction_user: Option<String>,
    pub password_env: String,
    #[serde(default)]
    pub use_ssh: bool,
    pub interval_seconds: u64,
}

/// What one owner maps to. A provider key that is absent is a hold (not mapped yet); `false` is a decision (no copy).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerMap {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gitlab: Option<Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitbucket: Option<Target>,
}

/// A namespace (GitLab group id, Bitbucket project key) or `false`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Target {
    Namespace(String),
    Off(bool),
    /// The copies live under another path than the primary's, and say why: the destination is pinned to
    /// `<path_prefix>/<repository>` (the owner's Bitbucket workspace is not the Forgejo organisation's name). Renaming the
    /// organisation later is one edit of `path_prefix`.
    Mapped {
        namespace: String,
        path_prefix: String,
        path_reason: String,
    },
}

/// One repository as the forge lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub id: u64,
    /// `owner/repo` as the forge spells it.
    pub path: String,
    pub private: bool,
    pub default_branch: String,
    pub empty: bool,
    pub fork: bool,
    pub mirror: bool,
}

/// An owner the policy cannot place on one provider yet. Listed once, with every repository it affects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hold {
    pub owner: String,
    pub provider: Provider,
    pub reason: String,
    pub repositories: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Outcome {
    /// Paths that got a derived row.
    pub derived: Vec<String>,
    pub holds: Vec<Hold>,
    /// Why a listed repository got no row, counted by reason.
    pub skipped: BTreeMap<&'static str, usize>,
}

impl Derive {
    pub fn validate(&self) -> Result<()> {
        for template in [&self.gitlab, &self.bitbucket].into_iter().flatten() {
            template.endpoint.validate()?;
            env_name(&template.password_env)?;
            if template.mirror_user.is_empty()
                || template.mirror_user.contains('@')
                || template.mirror_user.chars().any(char::is_control)
            {
                return Err(failure(
                    "Mirror principal must be an ID/login, never an email",
                ));
            }
            if template.interval_seconds < 600 {
                return Err(failure("Derived destination interval below ten minutes"));
            }
        }
        for (owner, map) in &self.owners {
            if owner.is_empty() || owner != &owner.to_ascii_lowercase() || owner.contains('/') {
                return Err(failure("Derive owners are lowercase logins"));
            }
            for target in [&map.gitlab, &map.bitbucket].into_iter().flatten() {
                match target {
                    Target::Off(true) => {
                        return Err(failure("A derive target is a namespace or false"))
                    }
                    Target::Namespace(ns) if ns.is_empty() || ns.chars().any(char::is_control) => {
                        return Err(failure("Empty derive namespace"))
                    }
                    Target::Mapped {
                        namespace,
                        path_prefix,
                        path_reason,
                    } if namespace.is_empty()
                        || super::component(path_prefix).is_err()
                        || path_reason.trim().is_empty()
                        || path_reason.chars().any(char::is_control) =>
                    {
                        return Err(failure(
                            "A mapped derive target needs a namespace, a path prefix and a reason",
                        ))
                    }
                    _ => {}
                }
            }
        }
        if self
            .exclude
            .iter()
            .any(|p| p.is_empty() || p != &p.to_ascii_lowercase())
        {
            return Err(failure(
                "Derive exclude patterns are lowercase and not empty",
            ));
        }
        if self
            .bitbucket_lfs_hold
            .as_deref()
            .is_some_and(|reason| reason.trim().is_empty() || reason.chars().any(char::is_control))
        {
            return Err(failure("The LFS hold needs a plain-text reason"));
        }
        Ok(())
    }

    fn template(&self, provider: Provider) -> Option<&Template> {
        match provider {
            Provider::Gitlab => self.gitlab.as_ref(),
            Provider::Bitbucket => self.bitbucket.as_ref(),
            Provider::Github => None,
        }
    }
}

fn destination(
    provider: Provider,
    template: &Template,
    namespace: &str,
    path: &str,
) -> Destination {
    Destination {
        provider,
        endpoint: template.endpoint.clone(),
        // Not pinned: it follows the primary, as a loaded destination does (`follow_primaries`).
        path: path.to_string(),
        path_reason: None,
        recorded_path: String::new(),
        namespace: namespace.to_string(),
        repository_id: None,
        mirror_user: template.mirror_user.clone(),
        restriction_user: template.restriction_user.clone(),
        password_env: template.password_env.clone(),
        use_ssh: template.use_ssh,
        interval_seconds: template.interval_seconds,
        sync_on_commit: true,
        branch_filter: String::new(),
        hold: None,
        absent: false,
        remote_name: None,
        lock_exception: None,
    }
}

/// `*` matches any run of characters (also none and also `/`); everything else matches itself.
pub fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if parts.len() == 1 {
        return pattern == text;
    }
    if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    true
}

/// True when the content of a `.gitattributes` file routes anything through Git LFS.
pub fn routes_lfs(gitattributes: &str) -> bool {
    gitattributes.lines().map(str::trim).any(|line| {
        !line.starts_with('#') && line.split_whitespace().any(|word| word == "filter=lfs")
    })
}

/// Adds a derived row for every listed repository the placement does not declare. `organizations` is the set of
/// lowercase organisation logins of the forge (a login outside it and outside `derive.owners` is somebody else's
/// account and is ignored). `lfs` is asked only for a repository that is about to get a row.
pub fn resolve(
    placement: &mut Placement,
    listing: &[Source],
    organizations: &BTreeSet<String>,
    lfs: &mut dyn FnMut(&Source) -> Result<bool>,
) -> Result<Outcome> {
    let mut outcome = Outcome::default();
    let Some(derive) = placement.derive.clone() else {
        return Ok(outcome);
    };
    derive.validate()?;
    let contributed: BTreeSet<String> = Contributed::from_placement(placement, &[])?
        .source_paths()
        .into_iter()
        .map(|p| p.to_ascii_lowercase())
        .collect();
    let declared_paths: BTreeSet<String> = placement
        .repositories
        .iter()
        .map(|r| r.path.to_ascii_lowercase())
        .collect();
    let declared_ids: BTreeSet<u64> = placement.repositories.iter().map(|r| r.source_id).collect();
    let mut holds: BTreeMap<(String, Provider), (String, Vec<String>)> = BTreeMap::new();
    let mut rows = Vec::new();
    let mut ordered: Vec<&Source> = listing.iter().collect();
    ordered.sort_by_key(|s| s.id);
    for source in ordered {
        let lower = source.path.to_ascii_lowercase();
        let mut skip = |why: &'static str| *outcome.skipped.entry(why).or_insert(0) += 1;
        if declared_paths.contains(&lower) || declared_ids.contains(&source.id) {
            skip("declared");
            continue;
        }
        if contributed.contains(&lower) {
            skip("contributed");
            continue;
        }
        if derive.exclude.iter().any(|pattern| glob(pattern, &lower)) {
            skip("excluded");
            continue;
        }
        if source.mirror {
            skip("pull-mirror");
            continue;
        }
        if source.fork {
            skip("fork");
            continue;
        }
        if source.empty {
            skip("empty");
            continue;
        }
        let Some((owner, name)) = lower.split_once('/') else {
            skip("malformed");
            continue;
        };
        if check_path(&source.path, false).is_err() || source.default_branch.is_empty() {
            skip("malformed");
            continue;
        }
        let map = derive.owners.get(owner);
        if map.is_none() && !organizations.contains(owner) {
            skip("unmapped-user");
            continue;
        }
        let profile = name == ".github";
        let mut destinations = Vec::new();
        for provider in [Provider::Gitlab, Provider::Bitbucket] {
            let Some(template) = derive.template(provider) else {
                continue;
            };
            if profile
                && derive
                    .profile_providers
                    .as_ref()
                    .is_some_and(|only| !only.contains(&provider))
            {
                continue;
            }
            let target = map.and_then(|m| match provider {
                Provider::Gitlab => m.gitlab.as_ref(),
                _ => m.bitbucket.as_ref(),
            });
            match target {
                Some(Target::Namespace(ns)) => {
                    destinations.push(destination(provider, template, ns, &source.path));
                }
                Some(Target::Mapped {
                    namespace,
                    path_prefix,
                    path_reason,
                }) => {
                    let mut dest = destination(
                        provider,
                        template,
                        namespace,
                        &format!("{path_prefix}/{name}"),
                    );
                    dest.path_reason = Some(path_reason.clone());
                    dest.recorded_path = dest.path.clone();
                    destinations.push(dest);
                }
                Some(Target::Off(_)) => {}
                None => {
                    let label = if provider == Provider::Gitlab {
                        "GitLab group"
                    } else {
                        "Bitbucket project"
                    };
                    let reason = if map.is_none() {
                        format!("organization {owner} has no mapping: create its {label} and add it to derive.owners")
                    } else {
                        format!("organization {owner} has no {label}: create it and add it to derive.owners")
                    };
                    holds
                        .entry((owner.to_string(), provider))
                        .or_insert_with(|| (reason, Vec::new()))
                        .1
                        .push(source.path.clone());
                }
            }
        }
        if destinations.is_empty() {
            skip("no-destination");
            continue;
        }
        let content = if profile {
            Content::Profile
        } else if lfs(source)? {
            Content::Lfs
        } else {
            Content::NativeGit
        };
        if content == Content::Lfs {
            for dest in destinations
                .iter_mut()
                .filter(|d| d.provider == Provider::Bitbucket)
            {
                if derive.bitbucket_lfs_ssh {
                    dest.use_ssh = true;
                } else {
                    dest.hold = derive.bitbucket_lfs_hold.clone();
                }
            }
        }
        outcome.derived.push(source.path.clone());
        rows.push(Repository {
            path: source.path.clone(),
            source_id: source.id,
            private: source.private,
            default_branch: source.default_branch.clone(),
            content,
            hold: None,
            destinations,
        });
    }
    placement.repositories.extend(rows);
    outcome.holds = holds
        .into_iter()
        .map(|((owner, provider), (reason, repositories))| Hold {
            owner,
            provider,
            reason,
            repositories,
        })
        .collect();
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn template(origin: &str, env: &str) -> Value {
        json!({"endpoint": {"origin": origin, "token_env": env}, "mirror_user": "3382224",
               "password_env": env, "use_ssh": false, "interval_seconds": 3600})
    }

    fn placement(declared: Value, contributed: Value) -> Placement {
        let document = json!({
            "schema": 1,
            "source": {"origin": "https://forge.example", "token_env": "FORGE"},
            "repositories": declared,
            "contributed": contributed,
            "derive": {
                "gitlab": template("https://gitlab.com", "GL"),
                "bitbucket": template("https://api.bitbucket.org", "BB"),
                "bitbucket_lfs_hold": "Bitbucket refuses LFS with the API token",
                "owners": {
                    "corbet-infra": {"gitlab": "111", "bitbucket": "CORE"},
                    "corbet-knowledge": {"gitlab": "222", "bitbucket": false},
                    "corbet-new": {"gitlab": "333"},
                    "julian-corbet": {"gitlab": "444", "bitbucket": false}
                }
            }
        });
        let p = Placement::from_document(document.to_string().as_bytes()).unwrap();
        p.validate().unwrap();
        p
    }

    fn src(id: u64, path: &str, private: bool) -> Source {
        Source {
            id,
            path: path.into(),
            private,
            default_branch: "main".into(),
            empty: false,
            fork: false,
            mirror: false,
        }
    }

    fn orgs() -> BTreeSet<String> {
        [
            "corbet-infra",
            "corbet-knowledge",
            "corbet-new",
            "corbet-stray",
        ]
        .map(String::from)
        .into()
    }

    fn run(p: &mut Placement, listing: &[Source], lfs: &[&str]) -> Outcome {
        let lfs: Vec<String> = lfs.iter().map(|s| s.to_string()).collect();
        resolve(p, listing, &orgs(), &mut |s| Ok(lfs.contains(&s.path))).unwrap()
    }

    fn row<'a>(p: &'a Placement, path: &str) -> &'a Repository {
        p.repositories
            .iter()
            .find(|r| r.path == path)
            .expect("derived row")
    }

    #[test]
    fn a_new_public_repository_gets_gitlab_and_bitbucket_copies() {
        let mut p = placement(json!([]), json!([]));
        let out = run(&mut p, &[src(10, "corbet-infra/cnew", false)], &[]);
        assert_eq!(out.derived, ["corbet-infra/cnew"]);
        let r = row(&p, "corbet-infra/cnew");
        assert!(!r.private);
        assert_eq!(r.content, Content::NativeGit);
        let d: Vec<_> = r
            .destinations
            .iter()
            .map(|d| (d.provider, d.namespace.as_str(), d.hold.is_some()))
            .collect();
        assert_eq!(
            d,
            [
                (Provider::Gitlab, "111", false),
                (Provider::Bitbucket, "CORE", false)
            ]
        );
        assert!(r
            .destinations
            .iter()
            .all(|d| !d.pinned() && d.recorded_path.is_empty()));
        p.validate().unwrap();
        assert_eq!(
            p.repositories[0].destination_path(&r.destinations[0]),
            "corbet-infra/cnew"
        );
    }

    #[test]
    fn a_private_repository_gets_private_copies() {
        let mut p = placement(json!([]), json!([]));
        run(&mut p, &[src(11, "corbet-infra/secret", true)], &[]);
        let r = row(&p, "corbet-infra/secret");
        assert!(r.private);
        assert_eq!(r.destinations.len(), 2);
    }

    #[test]
    fn lfs_content_is_recorded_and_bitbucket_waits_on_the_declared_hold() {
        let mut p = placement(json!([]), json!([]));
        run(
            &mut p,
            &[src(12, "corbet-infra/big", true)],
            &["corbet-infra/big"],
        );
        let r = row(&p, "corbet-infra/big");
        assert_eq!(r.content, Content::Lfs);
        let held: Vec<_> = r
            .destinations
            .iter()
            .map(|d| (d.provider, d.hold.is_some()))
            .collect();
        assert_eq!(
            held,
            [(Provider::Gitlab, false), (Provider::Bitbucket, true)]
        );
    }

    #[test]
    fn a_mapped_owner_pins_the_destination_under_its_workspace() {
        let mut p = placement(json!([]), json!([]));
        p.derive.as_mut().unwrap().owners.insert(
            "corbet-new".into(),
            OwnerMap {
                gitlab: Some(Target::Namespace("333".into())),
                bitbucket: Some(Target::Mapped {
                    namespace: "CORE".into(),
                    path_prefix: "cvector".into(),
                    path_reason:
                        "owner decision: the organisation's Bitbucket workspace is cvector".into(),
                }),
            },
        );
        run(&mut p, &[src(80, "corbet-new/Thing", false)], &[]);
        let r = row(&p, "corbet-new/Thing");
        assert_eq!(r.destinations[0].path, "corbet-new/Thing");
        let bb = &r.destinations[1];
        assert!(bb.pinned());
        assert_eq!(r.destination_path(bb), "cvector/thing");
        p.validate().unwrap();
    }

    #[test]
    fn lfs_over_ssh_needs_no_hold() {
        let mut p = placement(json!([]), json!([]));
        p.derive.as_mut().unwrap().bitbucket_lfs_ssh = true;
        run(
            &mut p,
            &[
                src(14, "corbet-infra/big", true),
                src(15, "corbet-infra/plain", true),
            ],
            &["corbet-infra/big"],
        );
        let d = &row(&p, "corbet-infra/big").destinations;
        assert_eq!((d[1].use_ssh, d[1].hold.is_none()), (true, true));
        assert!(!d[0].use_ssh);
        assert!(!row(&p, "corbet-infra/plain").destinations[1].use_ssh);
    }

    #[test]
    fn lfs_is_only_probed_for_a_repository_that_gets_a_row() {
        let mut p = placement(json!([]), json!([]));
        let mut asked = Vec::new();
        resolve(
            &mut p,
            &[
                src(1, "corbet-new/x", false),
                src(2, "corbet-infra/y", false),
                src(3, "someone/else", false),
            ],
            &orgs(),
            &mut |s| {
                asked.push(s.path.clone());
                Ok(false)
            },
        )
        .unwrap();
        assert_eq!(asked, ["corbet-new/x", "corbet-infra/y"]);
    }

    #[test]
    fn a_contributed_project_gets_nothing() {
        let contributed = json!([{"name": "arti", "upstream": "https://github.com/voltrevo/arti",
            "forks": ["https://forge.example/corbet-infra/arti"]}]);
        let mut p = placement(json!([]), contributed);
        let out = run(&mut p, &[src(13, "corbet-infra/Arti", false)], &[]);
        assert!(p.repositories.is_empty());
        assert_eq!(out.skipped.get("contributed"), Some(&1));
    }

    #[test]
    fn a_declared_row_overrides_the_derivation_even_with_no_destinations() {
        let declared = json!([{"path": "corbet-infra/kept", "source_id": 20, "private": true, "default_branch": "main",
            "content": "native-git", "hold": "decide the copy first", "destinations": []}]);
        let mut p = placement(declared, json!([]));
        // Listed under another case and, after a rename, under another name: the id still matches.
        let out = run(
            &mut p,
            &[
                src(20, "corbet-infra/Kept", true),
                src(20, "corbet-infra/renamed", true),
            ],
            &[],
        );
        assert_eq!(p.repositories.len(), 1);
        assert!(p.repositories[0].destinations.is_empty());
        assert_eq!(out.skipped.get("declared"), Some(&2));
        assert!(out.derived.is_empty());
    }

    #[test]
    fn a_deliberate_no_copy_is_not_a_hold_and_a_missing_mapping_is_listed_once() {
        let mut p = placement(json!([]), json!([]));
        let listing = [
            src(30, "corbet-knowledge/a", true),
            src(31, "corbet-new/a", false),
            src(32, "corbet-new/b", false),
            src(33, "corbet-stray/z", false),
        ];
        let out = run(&mut p, &listing, &[]);
        // knowledge: GitLab only (Bitbucket is false). new: GitLab only, Bitbucket is a hold. stray: hold on both.
        assert_eq!(row(&p, "corbet-knowledge/a").destinations.len(), 1);
        assert_eq!(row(&p, "corbet-new/b").destinations.len(), 1);
        assert!(p.repositories.iter().all(|r| r.path != "corbet-stray/z"));
        let holds: Vec<_> = out
            .holds
            .iter()
            .map(|h| (h.owner.as_str(), h.provider, h.repositories.len()))
            .collect();
        assert_eq!(
            holds,
            [
                ("corbet-new", Provider::Bitbucket, 2),
                ("corbet-stray", Provider::Gitlab, 1),
                ("corbet-stray", Provider::Bitbucket, 1)
            ]
        );
        assert!(out.holds[0].reason.contains("Bitbucket project"));
    }

    #[test]
    fn other_accounts_forks_pull_mirrors_and_empty_repositories_get_nothing() {
        let mut p = placement(json!([]), json!([]));
        let mut fork = src(41, "corbet-infra/forked", false);
        fork.fork = true;
        let mut mirror = src(42, "corbet-infra/pulled", false);
        mirror.mirror = true;
        let mut empty = src(43, "corbet-infra/nothing", false);
        empty.empty = true;
        let out = run(
            &mut p,
            &[src(40, "stranger/x", false), fork, mirror, empty],
            &[],
        );
        assert!(p.repositories.is_empty() && out.holds.is_empty());
        assert_eq!(out.skipped.len(), 4);
    }

    #[test]
    fn a_personal_login_is_placed_like_an_organization_and_profile_repositories_are_profile_content(
    ) {
        let mut p = placement(json!([]), json!([]));
        run(
            &mut p,
            &[
                src(50, "julian-corbet/tool", true),
                src(51, "corbet-infra/.github", false),
            ],
            &[],
        );
        let r = row(&p, "julian-corbet/tool");
        assert_eq!(r.destinations.len(), 1);
        assert_eq!(r.destinations[0].namespace, "444");
        assert_eq!(row(&p, "corbet-infra/.github").content, Content::Profile);
    }

    #[test]
    fn without_a_policy_nothing_changes_and_a_bad_policy_is_refused() {
        let mut p = placement(json!([]), json!([]));
        p.derive = None;
        assert!(run(&mut p, &[src(60, "corbet-infra/x", false)], &[])
            .derived
            .is_empty());
        let mut bad = placement(json!([]), json!([]));
        bad.derive
            .as_mut()
            .unwrap()
            .owners
            .insert("Mixed".into(), OwnerMap::default());
        assert!(bad.validate().is_err());
        let mut yes = placement(json!([]), json!([]));
        yes.derive
            .as_mut()
            .unwrap()
            .owners
            .get_mut("corbet-infra")
            .unwrap()
            .gitlab = Some(Target::Off(true));
        assert!(yes.validate().is_err());
    }

    #[test]
    fn scratch_repositories_are_excluded_and_profile_repositories_go_where_declared() {
        let mut p = placement(json!([]), json!([]));
        let derive = p.derive.as_mut().unwrap();
        derive.exclude = vec!["*/delete-me-*".into(), "corbet-new/tmp-*".into()];
        derive.profile_providers = Some(vec![Provider::Bitbucket]);
        let out = run(
            &mut p,
            &[
                src(70, "corbet-infra/delete-me-.github", false),
                src(71, "corbet-new/tmp-x", false),
                src(72, "corbet-infra/.github", false),
            ],
            &[],
        );
        assert_eq!(out.skipped.get("excluded"), Some(&2));
        let profile = row(&p, "corbet-infra/.github");
        assert_eq!(profile.destinations.len(), 1);
        assert_eq!(profile.destinations[0].provider, Provider::Bitbucket);
        // A profile repository of an owner with no Bitbucket project is only held there, not on GitLab.
        let mut q = placement(json!([]), json!([]));
        q.derive.as_mut().unwrap().profile_providers = Some(vec![Provider::Bitbucket]);
        let out = run(&mut q, &[src(73, "corbet-new/.github", false)], &[]);
        assert_eq!(out.holds.len(), 1);
        assert_eq!(out.holds[0].provider, Provider::Bitbucket);
    }

    #[test]
    fn globs_match_runs_and_anchor_both_ends() {
        assert!(glob("ds-*/*", "ds-probe/stress"));
        assert!(glob("*/delete-me-*", "corbet-labs/delete-me-.github"));
        assert!(glob("a*b*c", "aXbYc") && glob("a*b", "ab") && glob("*", "") && glob("x", "x"));
        assert!(
            !glob("a*b", "ba") && !glob("a*c", "ab") && !glob("x", "xy") && !glob("ab*ab", "ab")
        );
    }

    #[test]
    fn gitattributes_routing_is_recognised() {
        assert!(routes_lfs("*.bin filter=lfs diff=lfs merge=lfs -text\n"));
        assert!(!routes_lfs("# *.bin filter=lfs\n*.rs text eol=lf\n"));
    }
}
