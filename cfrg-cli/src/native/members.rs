//! `cfrg native --operation members`: what R43 asks of the copies' membership. It reads the
//! members of the primary's projects (non-owner teams and repository collaborators), applies
//! the one-way role rule (read on a private copy where the forge has nothing better, triage
//! everywhere else, never write) and reports the grants it would make and the members it
//! cannot place. Read-only: the identity table decides who has an account where, and no
//! handle is ever guessed from a name.
use super::Options;
use cfrg::{
    access::IdentityMap,
    contributed::Contributed,
    copy_access::{self, Copy},
    native::{http::Pacing, Provider},
    Result,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub fn run(options: &Options) -> Result<()> {
    if options.apply {
        return Err(
            "Member sync is read-only until the identity table is decided and reviewed".into(),
        );
    }
    let creds = super::creds::Creds::new(options.credentials_dir.clone());
    let placement = creds.placement(&std::fs::read(&options.placement)?)?;
    placement.validate()?;
    let mut io = creds.open(&options.state, false)?;
    // Our own forge: one read a second.
    io.set_pacing(Pacing {
        read: 1,
        write: 2,
        creation: 0,
    });
    let identities: IdentityMap = match &options.identities {
        Some(path) => toml::from_str(&std::fs::read_to_string(path)?)?,
        None => IdentityMap {
            schema: 1,
            people: BTreeMap::new(),
        },
    };
    // A contributed project (R10, R11) gets nothing of ours: not on the source, not on a copy.
    let contributed = Contributed::from_placement(&placement, &[])?;
    let primaries: Vec<_> = cfgj::native::list_primaries(&mut io, &placement.source)?
        .into_iter()
        .filter(|p| {
            !p.contributed
                && contributed
                    .classify(&placement.source.origin, &p.path)
                    .is_none()
        })
        .collect();
    let paths: Vec<String> = primaries.iter().map(|p| p.path.clone()).collect();
    let operator = options.operator.clone().ok_or_else(|| {
        Box::<dyn std::error::Error + Send + Sync>::from(
            "members: --operator LOGIN is required (the login that is never touched)",
        )
    })?;
    let mut members = cfgj::native::list_members(&mut io, &placement.source, &paths)?;
    members.retain(|m| !m.login.eq_ignore_ascii_case(&operator));

    // The copies: those the settings sweep found, else every copy R2 asks for.
    let existing: Option<BTreeSet<(String, String)>> = match &options.copies_from {
        Some(file) => {
            let report: Value = serde_json::from_slice(&std::fs::read(file)?)?;
            Some(
                report["copies"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|row| {
                        !matches!(
                            row["state"].as_str(),
                            Some("absent" | "error" | "contributed-project" | "provider-stopped")
                        )
                    })
                    .filter_map(|row| {
                        Some((
                            row["provider"].as_str()?.to_string(),
                            row["destination"].as_str()?.to_string(),
                        ))
                    })
                    .collect(),
            )
        }
        None => None,
    };
    let mut copies = Vec::new();
    for primary in &primaries {
        for (provider, name, host) in [
            (Provider::Github, "github", "github.com"),
            (Provider::Gitlab, "gitlab", "gitlab.com"),
            (Provider::Bitbucket, "bitbucket", "bitbucket.org"),
        ] {
            let path = placement
                .repositories
                .iter()
                .find(|r| r.source_id == primary.id)
                .and_then(|r| {
                    let mut live = r.clone();
                    live.path = primary.path.clone();
                    r.destinations
                        .iter()
                        .find(|d| d.provider == provider)
                        .map(|d| live.destination_path(d))
                })
                .unwrap_or_else(|| primary.path.clone());
            if existing
                .as_ref()
                .is_some_and(|set| !set.contains(&(name.to_string(), path.clone())))
                || contributed.classify(host, &path).is_some()
            {
                continue;
            }
            copies.push(Copy {
                provider,
                path,
                source: primary.path.clone(),
                private: primary.settings.private,
            });
        }
    }
    let plan = copy_access::plan(&members, &identities, &copies)?;

    let mut per_member = Vec::new();
    for member in &members {
        let mut orgs: BTreeMap<&str, usize> = BTreeMap::new();
        for repo in &member.repos {
            *orgs
                .entry(repo.split('/').next().unwrap_or_default())
                .or_default() += 1;
        }
        let covered = |name: &str| {
            copies
                .iter()
                .filter(|c| member.repos.contains(&c.source))
                .filter(|c| {
                    copy_access::copy_role(c.provider, c.private).is_some()
                        && match c.provider {
                            Provider::Github => name == "github",
                            Provider::Gitlab => name == "gitlab",
                            Provider::Bitbucket => name == "bitbucket",
                        }
                })
                .count()
        };
        per_member.push(json!({
            "login": member.login, "projects": member.repos.len(), "organisations": orgs,
            "copies_needing_a_grant": {"github":covered("github"), "gitlab":covered("gitlab"), "bitbucket":covered("bitbucket")},
        }));
    }
    let mut grants_by_provider: BTreeMap<&str, usize> = BTreeMap::new();
    for grant in &plan.grants {
        *grants_by_provider.entry(grant.provider).or_default() += 1;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "rule": "R43: read on a private copy, triage everywhere; never write or merge; one way, Forgejo to copies",
            "operator_excluded": operator,
            "copies_considered": copies.len(),
            "copies_source": if existing.is_some() { "copies found by the settings sweep" } else { "every copy R2 asks for" },
            "members": per_member,
            "grants_planned": grants_by_provider,
            "grants": plan.grants,
            "unplaced": plan.unplaced,
        }))?
    );
    Ok(())
}
