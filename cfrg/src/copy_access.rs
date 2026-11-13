//! R43 of the operating model: members on copies. A member of a project on Forgejo gets the
//! same membership on every forge where they have an account (the identity table says which
//! account), read on a private copy and triage everywhere. Nobody gets write or merge on a
//! copy: code enters a copy only through the mirroring, so no role this module can produce
//! may push.
//!
//! The flow is one way, from the primary to the copies. That is the difference from the
//! exact-mirror draft in [`crate::access`], which carries a grant from any forge to every
//! other one and so could carry Forgejo's `write` onto a copy. Pure: no network access.
use crate::{access::IdentityMap, native::Provider, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// What a member may be on a copy. There is deliberately no variant that can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CopyRole {
    /// Reads the code, including a private copy.
    Read,
    /// Reads, and manages issues and pull requests (labels, assignees, close, reopen).
    Triage,
}

/// The role a member gets on a copy of this visibility on this forge, or `None` when no
/// explicit grant is needed or possible.
///
/// GitHub has a triage role; GitLab's Reporter reads the code and manages issues. Bitbucket
/// has only read, write and admin: it offers read, which is all a copy may give there, and
/// only a private copy needs it (anybody reads a public one).
pub fn copy_role(provider: Provider, private: bool) -> Option<CopyRole> {
    match provider {
        Provider::Github | Provider::Gitlab => Some(CopyRole::Triage),
        Provider::Bitbucket => private.then_some(CopyRole::Read),
    }
}

/// The forge's own name for `role`.
pub fn native_role(provider: Provider, role: CopyRole) -> &'static str {
    match (provider, role) {
        (Provider::Github, CopyRole::Triage) => "triage",
        (Provider::Github, CopyRole::Read) => "pull",
        // GitLab access levels: Reporter 20, Guest 10.
        (Provider::Gitlab, CopyRole::Triage) => "reporter",
        (Provider::Gitlab, CopyRole::Read) => "reporter",
        (Provider::Bitbucket, _) => "read",
    }
}

/// A member of a project on the primary. `repos` are the `owner/repo` paths the membership
/// covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The login on Forgejo.
    pub login: String,
    pub repos: BTreeSet<String>,
}

/// One copy of a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    pub provider: Provider,
    /// `owner/repo` on that forge.
    pub path: String,
    /// The primary's path: what the membership is declared against.
    pub source: String,
    pub private: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Grant {
    pub provider: &'static str,
    pub copy: String,
    pub person: String,
    pub handle: String,
    pub role: CopyRole,
    pub native_role: &'static str,
}

/// Members the plan could not place, with the reason. Nothing is guessed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unplaced {
    pub login: String,
    pub provider: Option<&'static str>,
    pub reason: &'static str,
    pub copies: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct Plan {
    pub grants: Vec<Grant>,
    pub unplaced: Vec<Unplaced>,
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Github => "github",
        Provider::Gitlab => "gitlab",
        Provider::Bitbucket => "bitbucket",
    }
}

/// Decide the grants every copy needs for the primary's members. The identity table maps a
/// Forgejo login to the person's handle on each forge; a member without an entry, or
/// without an account on a forge, is reported and never guessed.
pub fn plan(members: &[Member], identities: &IdentityMap, copies: &[Copy]) -> Result<Plan> {
    let maps = identities.maps()?;
    // Forgejo login -> person id.
    let person_of: BTreeMap<String, String> = members
        .iter()
        .filter_map(|m| {
            maps.person_for("forgejo", &m.login)
                .map(|person| (m.login.to_ascii_lowercase(), person.to_string()))
        })
        .collect();
    let mut plan = Plan::default();
    let mut unplaced: BTreeMap<(String, Option<&'static str>, &'static str), usize> =
        BTreeMap::new();
    for member in members {
        let covered: Vec<&Copy> = copies
            .iter()
            .filter(|c| member.repos.contains(&c.source))
            .collect();
        let Some(person) = person_of.get(&member.login.to_ascii_lowercase()) else {
            *unplaced
                .entry((member.login.clone(), None, "no-identity-table-entry"))
                .or_default() += covered.len();
            continue;
        };
        for copy in covered {
            let Some(role) = copy_role(copy.provider, copy.private) else {
                continue;
            };
            match maps.handle_for(person, provider_name(copy.provider)) {
                Some(handle) => plan.grants.push(Grant {
                    provider: provider_name(copy.provider),
                    copy: copy.path.clone(),
                    person: person.clone(),
                    handle: handle.to_string(),
                    role,
                    native_role: native_role(copy.provider, role),
                }),
                None => {
                    *unplaced
                        .entry((
                            member.login.clone(),
                            Some(provider_name(copy.provider)),
                            "no-account-on-this-forge",
                        ))
                        .or_default() += 1;
                }
            }
        }
    }
    plan.unplaced = unplaced
        .into_iter()
        .map(|((login, provider, reason), copies)| Unplaced {
            login,
            provider,
            reason,
            copies,
        })
        .collect();
    plan.grants
        .sort_by(|a, b| (a.provider, &a.copy, &a.handle).cmp(&(b.provider, &b.copy, &b.handle)));
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identities() -> IdentityMap {
        toml::from_str(
            r#"schema = 1
[people.carmen]
forgejo = "carmenb"
github = "carmen-gh"
gitlab = "carmen-gl"
"#,
        )
        .unwrap()
    }
    fn copy(provider: Provider, path: &str, private: bool) -> Copy {
        Copy {
            provider,
            path: path.into(),
            source: "org/repo".into(),
            private,
        }
    }
    fn member(login: &str) -> Member {
        Member {
            login: login.into(),
            repos: ["org/repo".to_string()].into(),
        }
    }

    #[test]
    fn no_role_a_copy_can_give_may_write_and_triage_is_the_most() {
        // The type has no write variant; the strongest native names are triage and reporter.
        for provider in [Provider::Github, Provider::Gitlab, Provider::Bitbucket] {
            for private in [false, true] {
                if let Some(role) = copy_role(provider, private) {
                    let name = native_role(provider, role);
                    assert!(
                        ![
                            "push",
                            "write",
                            "maintain",
                            "admin",
                            "developer",
                            "maintainer"
                        ]
                        .contains(&name),
                        "{provider:?} {name}"
                    );
                }
            }
        }
        assert_eq!(copy_role(Provider::Github, false), Some(CopyRole::Triage));
        assert_eq!(copy_role(Provider::Gitlab, true), Some(CopyRole::Triage));
    }

    #[test]
    fn bitbucket_gives_read_on_a_private_copy_only() {
        assert_eq!(copy_role(Provider::Bitbucket, true), Some(CopyRole::Read));
        assert_eq!(copy_role(Provider::Bitbucket, false), None);
    }

    #[test]
    fn a_forgejo_member_gets_triage_on_each_copy_where_they_have_an_account() {
        let copies = [
            copy(Provider::Github, "org/repo", false),
            copy(Provider::Gitlab, "org/repo", true),
            copy(Provider::Bitbucket, "org/repo", true),
        ];
        let plan = plan(&[member("carmenb")], &identities(), &copies).unwrap();
        let names: Vec<_> = plan
            .grants
            .iter()
            .map(|g| (g.provider, g.handle.as_str(), g.native_role))
            .collect();
        assert_eq!(
            names,
            [
                ("github", "carmen-gh", "triage"),
                ("gitlab", "carmen-gl", "reporter")
            ]
        );
        // No Bitbucket account: reported, not guessed.
        assert_eq!(plan.unplaced.len(), 1);
        assert_eq!(plan.unplaced[0].provider, Some("bitbucket"));
        assert_eq!(plan.unplaced[0].reason, "no-account-on-this-forge");
    }

    #[test]
    fn a_member_without_an_identity_entry_is_never_matched_by_name() {
        let copies = [copy(Provider::Github, "org/repo", false)];
        let plan = plan(&[member("lisah")], &identities(), &copies).unwrap();
        assert!(plan.grants.is_empty());
        assert_eq!(plan.unplaced[0].reason, "no-identity-table-entry");
        assert_eq!(plan.unplaced[0].copies, 1);
    }

    #[test]
    fn membership_covers_only_the_projects_it_is_declared_on() {
        let mut other = copy(Provider::Github, "org/other", false);
        other.source = "org/other".into();
        let copies = [copy(Provider::Github, "org/repo", false), other];
        let plan = plan(&[member("carmenb")], &identities(), &copies).unwrap();
        assert_eq!(plan.grants.len(), 1);
        assert_eq!(plan.grants[0].copy, "org/repo");
    }
}
