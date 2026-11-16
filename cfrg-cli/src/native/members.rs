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
    let apply = options.apply;
    let creds = super::creds::Creds::new(options.credentials_dir.clone());
    let mut placement = creds.placement(&std::fs::read(&options.placement)?)?;
    placement.validate()?;
    let mut io = creds.open(&options.state, apply)?;
    super::derive::apply(&creds, &mut placement, &mut io, None)?;
    placement.validate()?;
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
    let applied = if apply {
        // Another credential than the App's: a collaborator write needs Administration: write, which the App does not hold (R65).
        io.set_pacing(Pacing {
            read: 2,
            write: 120,
            creation: 0,
        });
        apply_github(&mut io, &plan.grants, options.max_writes.unwrap_or(5))?
    } else {
        Vec::new()
    };
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
            "applied": applied,
        }))?
    );
    Ok(())
}

/// The credential of the GitHub member writes: the owner's token (the gate's write credential), never the App's.
const MEMBERS_TOKEN_ENV: &str = "CFRG_GITHUB_MEMBERS_TOKEN";

fn github_request(
    method: &'static str,
    path: String,
    body: Option<Value>,
) -> cfrg::native::http::Request {
    cfrg::native::http::Request {
        endpoint: cfrg::native::Endpoint {
            origin: "https://api.github.com".into(),
            token_env: MEMBERS_TOKEN_ENV.into(),
        },
        auth: cfrg::native::http::Auth::Bearer,
        method,
        path,
        body,
        scope: "https://api.github.com/members".into(),
        creation: false,
    }
}

/// Whether the permission a collaborator already holds is at least triage (never lowered, never touched).
fn at_least_triage(permission: &str) -> bool {
    matches!(permission, "triage" | "write" | "maintain" | "admin")
}

/// Applies the GitHub grants of the plan: triage for the handle, one paced write each, at most `max` writes per run. A copy
/// that is not on GitHub yet, or a handle that already holds triage or more, is reported and not written. Only GitHub grants
/// are applied here (the identity table of 2026-11-16 maps GitHub handles); a stop of the transport ends the run.
fn apply_github(
    io: &mut dyn cfrg::native::http::Transport,
    grants: &[copy_access::Grant],
    max: usize,
) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    let mut written = 0usize;
    for grant in grants.iter().filter(|g| g.provider == "github") {
        let row = |state: &str| json!({"copy": grant.copy, "handle": grant.handle, "role": grant.native_role, "state": state});
        if grant.role != copy_access::CopyRole::Triage && grant.role != copy_access::CopyRole::Read
        {
            continue;
        }
        let path = format!("/repos/{}/collaborators/{}", grant.copy, grant.handle);
        let seen = io.send(github_request("GET", format!("{path}/permission"), None));
        let seen = match seen {
            Ok(response) => response,
            Err(error) if error.to_string().contains(cfrg::native::http::REFUSED_MARK) => {
                rows.push(row("refused"));
                continue;
            }
            Err(error) => return Err(error),
        };
        match seen.status {
            200 => {
                // `role_name` is the real role (triage, maintain, ...); the legacy `permission` folds triage into read.
                let held = seen.body["role_name"]
                    .as_str()
                    .or(seen.body["permission"].as_str())
                    .unwrap_or("none");
                if at_least_triage(held) {
                    rows.push(row("already"));
                    continue;
                }
            }
            404 => {
                rows.push(row("copy-absent"));
                continue;
            }
            _ => {
                rows.push(row("unexpected-read"));
                continue;
            }
        }
        if written >= max {
            rows.push(row("write-budget-spent"));
            continue;
        }
        written += 1;
        let put = io.send(github_request(
            "PUT",
            path,
            Some(json!({"permission": grant.native_role})),
        ));
        match put {
            // 201: an invitation was created; 204: the person already is a collaborator and the role was set.
            Ok(response) if matches!(response.status, 201 | 204) => {
                rows.push(row(if response.status == 201 {
                    "invited"
                } else {
                    "applied"
                }))
            }
            Ok(_) => rows.push(row("unexpected-write")),
            Err(error) if error.to_string().contains(cfrg::native::http::REFUSED_MARK) => {
                rows.push(row("refused"))
            }
            Err(error) => return Err(error),
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfrg::native::http::{Request, Response, Transport};

    struct Seq(Vec<(&'static str, u16, Value)>, Vec<String>);
    impl Transport for Seq {
        fn send(&mut self, request: Request) -> Result<Response> {
            self.1.push(format!("{} {}", request.method, request.path));
            let (method, status, body) = self.0.remove(0);
            assert_eq!(method, request.method);
            Ok(Response { status, body })
        }
    }

    fn grant(copy: &str, handle: &str) -> copy_access::Grant {
        copy_access::Grant {
            provider: "github",
            copy: copy.into(),
            person: "p".into(),
            handle: handle.into(),
            role: copy_access::CopyRole::Triage,
            native_role: "triage",
        }
    }

    #[test]
    fn triage_is_written_only_where_it_is_missing_and_never_lowered() {
        let grants = vec![
            grant("o/a", "h"),
            grant("o/b", "h"),
            grant("o/c", "h"),
            grant("o/d", "h"),
        ];
        let mut io = Seq(
            vec![
                (
                    "GET",
                    200,
                    json!({"permission": "read", "role_name": "read"}),
                ),
                ("PUT", 201, Value::Null),
                (
                    "GET",
                    200,
                    json!({"permission": "read", "role_name": "triage"}),
                ),
                ("GET", 404, Value::Null),
                ("GET", 200, json!({"permission": "none"})),
            ],
            vec![],
        );
        // budget of one write: o/a is invited, o/b is left alone, o/c is absent, o/d waits for the next run
        let rows = apply_github(&mut io, &grants, 1).unwrap();
        let states: Vec<_> = rows.iter().map(|r| r["state"].as_str().unwrap()).collect();
        assert_eq!(
            states,
            ["invited", "already", "copy-absent", "write-budget-spent"]
        );
        assert_eq!(io.1.iter().filter(|c| c.starts_with("PUT")).count(), 1);
        assert!(io.1[1].starts_with("PUT /repos/o/a/collaborators/h"));
    }

    #[test]
    fn the_role_written_is_never_above_triage() {
        assert!(at_least_triage("admin") && at_least_triage("triage") && !at_least_triage("read"));
        assert_eq!(
            copy_access::native_role(Provider::Github, copy_access::CopyRole::Triage),
            "triage"
        );
    }
}
