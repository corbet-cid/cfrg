//! Moving a destination between namespaces: the capability `transfer`.
//!
//! A destination follows the primary's `<org>/<repo>` (through the placement's `org_map`). When the namespace part
//! changes, the destination is moved by its provider's own means, never forced and never repeated blindly:
//!
//! | Provider | How |
//! |---|---|
//! | GitLab | `PUT /projects/:id/transfer` to the target group. The id stays, the old path redirects. |
//! | GitHub | `POST /repos/:owner/:repo/transfer` to the target owner, paced as a creation. The caller holds the write gate. |
//! | Bitbucket | There is no public cross-workspace transfer. A new repository is created in the target workspace (project key
//! from `namespace`), marked with the old repository's identity; the mirror fills it; when EVERY branch and tag matches the old one
//! exactly, the old repository is deleted. Each pass does one step, so a crash between steps resumes. |
//!
//! `plan` reports `transfer-planned`, `reconcile --apply` reports `transferred` (or `transfer-pending` while Bitbucket waits for
//! its mirror). Everything here talks through [`Transport`]: the tests use a recorded fake and never the network.
use super::{
    encode,
    http::{expect, Auth, Request, Response, Transport},
    Destination, Provider,
};
use crate::{failure, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// What one pass did (or would do) to move a destination.
#[derive(Debug, Clone, PartialEq)]
pub enum Transfer {
    /// Dry run: nothing was written. `steps` says what `--apply` would do, in order.
    Planned { from: String, steps: Vec<String> },
    /// The destination is at the wanted path now. `detail` carries what the placement must learn (a new Bitbucket uuid).
    Done { from: String, detail: Value },
    /// A step was taken (or is waited for); the next pass continues. Nothing is lost meanwhile.
    Pending {
        from: String,
        detail: String,
        /// The identity of the destination at its new path, once it exists (Bitbucket's uuid).
        new_repository_id: Option<String>,
    },
    /// Cannot proceed; reported, never forced.
    Blocked { from: String, reason: String },
}

fn blocked(from: &str, reason: impl Into<String>) -> Result<Transfer> {
    Ok(Transfer::Blocked {
        from: from.to_string(),
        reason: reason.into(),
    })
}

fn split(path: &str) -> Result<(&str, &str)> {
    path.rsplit_once('/')
        .ok_or_else(|| failure("Invalid repository path"))
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The marker a Bitbucket target carries while it is being filled: it is how a later pass knows the repository is the one this
/// move created, and not somebody else's repository that happens to have the wanted name.
pub fn bitbucket_marker(old_uuid: &str) -> String {
    format!("cfrg-transfer-from:{old_uuid}")
}

// ---------------------------------------------------------------------------------------------------------------------
// GitLab

fn gitlab_call(
    io: &mut dyn Transport,
    dest: &Destination,
    method: &'static str,
    path: String,
    body: Option<Value>,
) -> Result<Response> {
    if dest.provider != Provider::Gitlab {
        return Err(failure("GitLab transfer requires a GitLab destination"));
    }
    io.send(Request {
        endpoint: dest.endpoint.clone(),
        auth: Auth::PrivateToken,
        method,
        path: format!("/api/v4{path}"),
        body,
        scope: format!("{}/api", dest.endpoint.origin),
        creation: false,
    })
}

/// Move a GitLab project (`dest.repository_id`, now at `from`) into the namespace of `dest.path`.
pub fn gitlab(
    io: &mut dyn Transport,
    dest: &Destination,
    from: &str,
    apply: bool,
) -> Result<Transfer> {
    let id: u64 = dest
        .repository_id
        .as_deref()
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| failure("GitLab repository_id must be a numeric project ID"))?;
    let (live_ns, live_leaf) = split(from)?;
    let (wanted_ns, wanted_leaf) = split(&dest.path)?;
    if same(live_ns, wanted_ns) {
        return Err(failure(
            "GitLab transfer asked for a move inside one namespace",
        ));
    }
    if !same(live_leaf, wanted_leaf) {
        return blocked(
            from,
            "the namespace and the name both differ; transfer first, then rename",
        );
    }
    let group = gitlab_call(
        io,
        dest,
        "GET",
        format!("/groups/{}", encode(wanted_ns)),
        None,
    )?;
    let group_id = match group.status {
        200 => group.body["id"]
            .as_u64()
            .ok_or_else(|| failure("Missing GitLab group id"))?,
        404 => return blocked(from, "the target group does not exist"),
        other => {
            return Err(failure(format!(
                "GitLab group lookup failed with HTTP {other}"
            )))
        }
    };
    let taken = gitlab_call(
        io,
        dest,
        "GET",
        format!("/projects/{}", encode(&dest.path)),
        None,
    )?;
    match taken.status {
        // 301: only a redirect that an earlier project of that name left behind.
        404 | 301 => {}
        200 if taken.body["id"].as_u64() == Some(id) => {
            return Ok(Transfer::Done {
                from: from.to_string(),
                detail: json!({"repository_id": id.to_string()}),
            })
        }
        200 => return blocked(from, "the wanted path is already taken in the target group"),
        other => {
            return Err(failure(format!(
                "GitLab path lookup failed with HTTP {other}"
            )))
        }
    }
    if !apply {
        return Ok(Transfer::Planned {
            from: from.to_string(),
            steps: vec![format!(
                "PUT /projects/{id}/transfer namespace={group_id} ({wanted_ns}); the id stays and {from} redirects"
            )],
        });
    }
    let response = gitlab_call(
        io,
        dest,
        "PUT",
        format!("/projects/{id}/transfer"),
        Some(json!({"namespace": group_id})),
    )?;
    match response.status {
        200 if response.body["path_with_namespace"]
            .as_str()
            .is_some_and(|p| same(p, &dest.path)) =>
        {
            Ok(Transfer::Done {
                from: from.to_string(),
                detail: json!({"repository_id": id.to_string()}),
            })
        }
        200 => Err(failure(
            "GitLab transfer not confirmed; inspect the project before retrying",
        )),
        400 | 403 | 404 | 409 | 422 => blocked(
            from,
            format!(
                "GitLab refused the transfer: {}",
                response.body["message"]
                    .as_str()
                    .unwrap_or("no reason given")
            ),
        ),
        other => Err(failure(format!(
            "GitLab transfer failed with HTTP {other}; inspect the project before retrying"
        ))),
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// GitHub

fn github_call(
    io: &mut dyn Transport,
    dest: &Destination,
    method: &'static str,
    path: &str,
    body: Option<Value>,
    creation: bool,
) -> Result<Response> {
    if dest.provider != Provider::Github || dest.endpoint.origin != "https://api.github.com" {
        return Err(failure("GitHub transfer requires the GitHub API origin"));
    }
    // The owner scopes the token; a transfer is paced like a creation (the longest spacing).
    io.send(Request {
        endpoint: dest.endpoint.clone(),
        auth: Auth::Token,
        method,
        path: path.to_string(),
        body,
        scope: format!(
            "{}/{}",
            dest.endpoint.origin,
            if creation { "create-repo" } else { "api" }
        ),
        creation,
    })
}

/// Why the gate refuses a GitHub write at the moment, if it does. The caller reads the gate's files (kill switch, pause
/// directory, flagged organisations); this module only decides on the answer.
#[derive(Debug, Clone, Default)]
pub struct Gate {
    pub refusal: Option<String>,
}

/// Move a GitHub repository (`dest.repository_id`, now at `from`) to the owner of `dest.path`.
pub fn github(
    io: &mut dyn Transport,
    dest: &Destination,
    from: &str,
    apply: bool,
    gate: &Gate,
) -> Result<Transfer> {
    let id: u64 = dest
        .repository_id
        .as_deref()
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| failure("GitHub repository_id must be a numeric repository ID"))?;
    let (live_owner, live_leaf) = split(from)?;
    let (wanted_owner, wanted_leaf) = split(&dest.path)?;
    if same(live_owner, wanted_owner) {
        return Err(failure("GitHub transfer asked for a move inside one owner"));
    }
    if !same(live_leaf, wanted_leaf) {
        return blocked(
            from,
            "the owner and the name both differ; GitHub names are changed in GitHub",
        );
    }
    if let Some(reason) = &gate.refusal {
        return blocked(from, format!("the GitHub write gate holds: {reason}"));
    }
    let target = github_call(
        io,
        dest,
        "GET",
        &format!("/orgs/{}", encode(wanted_owner)),
        None,
        false,
    )?;
    match target.status {
        200 => {}
        404 => {
            return blocked(
                from,
                "the target owner is not a GitHub organisation we can reach",
            )
        }
        other => {
            return Err(failure(format!(
                "GitHub owner lookup failed with HTTP {other}"
            )))
        }
    }
    let taken = github_call(
        io,
        dest,
        "GET",
        &format!("/repos/{}", dest.path),
        None,
        false,
    )?;
    match taken.status {
        404 => {}
        200 if taken.body["id"].as_u64() == Some(id) => {
            return Ok(Transfer::Done {
                from: from.to_string(),
                detail: json!({"repository_id": id.to_string()}),
            })
        }
        200 => return blocked(from, "the wanted name is already taken at the target owner"),
        other => {
            return Err(failure(format!(
                "GitHub name lookup failed with HTTP {other}"
            )))
        }
    }
    if !apply {
        return Ok(Transfer::Planned {
            from: from.to_string(),
            steps: vec![format!(
                "POST /repos/{from}/transfer new_owner={wanted_owner} through the write gate (paced as a creation)"
            )],
        });
    }
    let response = github_call(
        io,
        dest,
        "POST",
        &format!("/repos/{from}/transfer"),
        Some(json!({"new_owner": wanted_owner})),
        true,
    )?;
    match response.status {
        200..=202 => {}
        403 | 404 | 422 => {
            return blocked(
                from,
                format!(
                    "GitHub refused the transfer: {}",
                    response.body["message"]
                        .as_str()
                        .unwrap_or("no reason given")
                ),
            )
        }
        other => {
            return Err(failure(format!(
                "GitHub transfer failed with HTTP {other}; inspect the repository before retrying"
            )))
        }
    }
    // Read it back by id: only the live name proves the move.
    let back = github_call(io, dest, "GET", &format!("/repositories/{id}"), None, false)?;
    let live = expect(back, &[200])?;
    if live["full_name"]
        .as_str()
        .is_some_and(|n| same(n, &dest.path))
    {
        Ok(Transfer::Done {
            from: from.to_string(),
            detail: json!({"repository_id": id.to_string()}),
        })
    } else {
        Ok(Transfer::Pending {
            from: from.to_string(),
            detail: "GitHub accepted the transfer; the repository is not at the new owner yet"
                .into(),
            new_repository_id: None,
        })
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Bitbucket

fn bitbucket_call(
    io: &mut dyn Transport,
    dest: &Destination,
    method: &'static str,
    path: String,
    body: Option<Value>,
    creation: bool,
) -> Result<Response> {
    if dest.provider != Provider::Bitbucket || dest.endpoint.origin != "https://api.bitbucket.org" {
        return Err(failure("Bitbucket Cloud API origin required"));
    }
    let workspace = dest
        .path
        .split('/')
        .next()
        .ok_or_else(|| failure("Missing workspace"))?;
    io.send(Request {
        endpoint: dest.endpoint.clone(),
        auth: Auth::Bearer,
        method,
        path,
        body,
        scope: format!("{}/workspace/{workspace}", dest.endpoint.origin),
        creation,
    })
}

/// Every branch and tag of `full_name` as name to commit hash, following the pagination.
pub fn bitbucket_refs(
    io: &mut dyn Transport,
    dest: &Destination,
    full_name: &str,
) -> Result<BTreeMap<String, String>> {
    let mut refs = BTreeMap::new();
    let mut next = Some(format!("/2.0/repositories/{full_name}/refs?pagelen=100"));
    let mut pages = 0;
    while let Some(path) = next.take() {
        pages += 1;
        if pages > 200 {
            return Err(failure("Bitbucket ref listing does not end"));
        }
        let page = expect(bitbucket_call(io, dest, "GET", path, None, false)?, &[200])?;
        for value in page["values"].as_array().into_iter().flatten() {
            let name = value["name"]
                .as_str()
                .ok_or_else(|| failure("Missing Bitbucket ref name"))?;
            let kind = value["type"].as_str().unwrap_or("ref");
            let hash = value["target"]["hash"]
                .as_str()
                .ok_or_else(|| failure("Missing Bitbucket ref target"))?;
            refs.insert(format!("{kind}:{name}"), hash.to_string());
        }
        next = page["next"].as_str().map(|url| {
            url.strip_prefix(dest.endpoint.origin.trim_end_matches('/'))
                .unwrap_or(url)
                .to_string()
        });
    }
    Ok(refs)
}

/// The refs that differ between `old` and `new`, as readable findings (empty: identical, every ref).
pub fn compare_refs(old: &BTreeMap<String, String>, new: &BTreeMap<String, String>) -> Vec<String> {
    let mut findings = Vec::new();
    for (name, hash) in old {
        match new.get(name) {
            None => findings.push(format!("{name}: missing in the new repository")),
            Some(other) if other != hash => findings.push(format!("{name}: differs")),
            Some(_) => {}
        }
    }
    for name in new.keys().filter(|name| !old.contains_key(*name)) {
        findings.push(format!("{name}: only in the new repository"));
    }
    findings
}

/// What the move needs besides the destination. `project_key` is the Bitbucket project the new repository goes into.
pub struct BitbucketMove<'a> {
    pub from: &'a str,
    pub old_uuid: &'a str,
    pub project_key: &'a str,
    pub private: bool,
}

/// One step of the Bitbucket move: create the target, wait for the mirror, delete the old repository after an exact match.
pub fn bitbucket(
    io: &mut dyn Transport,
    dest: &Destination,
    mv: &BitbucketMove,
    apply: bool,
) -> Result<Transfer> {
    let from = mv.from;
    let (live_ws, live_slug) = split(from)?;
    let (wanted_ws, wanted_slug) = split(&dest.path)?;
    if same(live_ws, wanted_ws) {
        return Err(failure("Bitbucket move asked inside one workspace"));
    }
    if !same(live_slug, wanted_slug) {
        return blocked(
            from,
            "the workspace and the name both differ; move first, then rename",
        );
    }
    if mv.project_key.is_empty() {
        return blocked(
            from,
            "the destination names no Bitbucket project key (namespace)",
        );
    }
    let marker = bitbucket_marker(mv.old_uuid);
    let target = bitbucket_call(
        io,
        dest,
        "GET",
        format!("/2.0/repositories/{}", dest.path),
        None,
        false,
    )?;
    let new_uuid = match target.status {
        404 => {
            if !apply {
                return Ok(Transfer::Planned {
                    from: from.to_string(),
                    steps: vec![
                        format!(
                            "create {} in project {} ({}), marked {marker}",
                            dest.path,
                            mv.project_key,
                            if mv.private { "private" } else { "public" }
                        ),
                        "mirror it from Forgejo".into(),
                        format!("compare every branch and tag of {from} and {}", dest.path),
                        format!("delete {from} only after the compare matches"),
                    ],
                });
            }
            let created = bitbucket_call(
                io,
                dest,
                "POST",
                format!("/2.0/repositories/{}", dest.path),
                Some(json!({
                    "scm": "git",
                    "is_private": mv.private,
                    "project": {"key": mv.project_key},
                    "description": marker,
                })),
                true,
            )?;
            let body = expect(created, &[200, 201])?;
            let uuid = body["uuid"].as_str().map(String::from);
            return Ok(Transfer::Pending {
                from: from.to_string(),
                detail: "created the new repository; the mirror must carry every ref before the old one goes".into(),
                new_repository_id: uuid,
            });
        }
        200 => {
            if target.body["description"].as_str() != Some(marker.as_str()) {
                return blocked(
                    from,
                    "the wanted name is taken by a repository this move did not create",
                );
            }
            target.body["uuid"].as_str().map(String::from)
        }
        other => {
            return Err(failure(format!(
                "Bitbucket name lookup failed with HTTP {other}"
            )))
        }
    };
    let old = bitbucket_refs(io, dest, from)?;
    let new = bitbucket_refs(io, dest, &dest.path)?;
    let findings = compare_refs(&old, &new);
    if !findings.is_empty() {
        let first: Vec<_> = findings.iter().take(5).cloned().collect();
        return Ok(Transfer::Pending {
            from: from.to_string(),
            detail: format!(
                "{} of {} refs do not match yet ({}); the old repository stays",
                findings.len(),
                old.len(),
                first.join("; ")
            ),
            new_repository_id: new_uuid,
        });
    }
    if !apply {
        return Ok(Transfer::Planned {
            from: from.to_string(),
            steps: vec![format!(
                "all {} refs match; delete {from} (the old repository)",
                old.len()
            )],
        });
    }
    let deleted = bitbucket_call(
        io,
        dest,
        "DELETE",
        format!("/2.0/repositories/{from}"),
        None,
        false,
    )?;
    expect(deleted, &[200, 202, 204])?;
    Ok(Transfer::Done {
        from: from.to_string(),
        detail: json!({"repository_id": new_uuid, "refs_compared": old.len()}),
    })
}

#[cfg(test)]
mod tests;
