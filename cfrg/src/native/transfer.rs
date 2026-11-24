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
    http::{expect, Auth, Request, Response, Transport, REFUSED_MARK},
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

/// A write the provider refused for this request only (the transport turns a 403 into an error carrying `REFUSED_MARK`) is a
/// blocked move, never a failure of the run. Every other error propagates: it may stop the run.
fn writing(result: Result<Response>) -> Result<std::result::Result<Response, String>> {
    match result {
        Err(error) if error.to_string().contains(REFUSED_MARK) => {
            Ok(Err("the provider refused the request (HTTP 403)".into()))
        }
        other => other.map(Ok),
    }
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
    // The namespace moves first, under the name the project has; a different wanted name is the rename of the next pass.
    let landing = format!("{wanted_ns}/{live_leaf}");
    let renames = !same(live_leaf, wanted_leaf);
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
        format!("/projects/{}", encode(&landing)),
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
            steps: {
                let mut steps = vec![format!(
                    "PUT /projects/{id}/transfer namespace={group_id} ({wanted_ns}); the id stays and {from} redirects"
                )];
                if renames {
                    steps.push(format!(
                        "then (next pass) rename {landing} to {}",
                        dest.path
                    ));
                }
                steps
            },
        });
    }
    let response = match writing(gitlab_call(
        io,
        dest,
        "PUT",
        format!("/projects/{id}/transfer"),
        Some(json!({"namespace": group_id})),
    ))? {
        Ok(response) => response,
        Err(reason) => return blocked(from, reason),
    };
    match response.status {
        200 if response.body["path_with_namespace"]
            .as_str()
            .is_some_and(|p| same(p, &landing)) =>
        {
            io.resolve(&format!("{}/api", dest.endpoint.origin))?;
            if renames {
                return Ok(Transfer::Pending {
                    from: from.to_string(),
                    detail: format!(
                        "transferred to {landing}; the rename to {} follows in the next pass",
                        dest.path
                    ),
                    new_repository_id: None,
                });
            }
            Ok(Transfer::Done {
                from: from.to_string(),
                detail: json!({"repository_id": id.to_string()}),
            })
        }
        200 => Err(failure(
            "GitLab transfer not confirmed; inspect the project before retrying",
        )),
        status @ (400 | 403 | 404 | 409 | 422) => {
            blocked(from, format!("GitLab refused the transfer (HTTP {status})"))
        }
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
///
/// `requested`: this transfer was already posted by an earlier pass and has not shown up yet; it is read back, never posted again.
/// The one POST is announced to the transport with `note` just before it is sent (the pacing and the request are recorded in
/// the durable state), so a crash after the POST can never repeat it; a refusal or a success clears the record.
pub fn github(
    io: &mut dyn Transport,
    dest: &Destination,
    from: &str,
    apply: bool,
    gate: &Gate,
    requested: bool,
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
    // GitHub moves and renames in ONE request (`new_owner` with `new_name`): one write, whatever the name does.
    let new_name = (!same(live_leaf, wanted_leaf)).then_some(wanted_leaf);
    if let Some(reason) = &gate.refusal {
        return blocked(from, format!("the GitHub write gate holds: {reason}"));
    }
    let done = |from: &str| Transfer::Done {
        from: from.to_string(),
        detail: json!({"repository_id": id.to_string()}),
    };
    if requested {
        let live = expect(
            github_call(io, dest, "GET", &format!("/repositories/{id}"), None, false)?,
            &[200],
        )?;
        return Ok(
            if live["full_name"]
                .as_str()
                .is_some_and(|n| same(n, &dest.path))
            {
                io.note("github-transfer-clear", &id.to_string(), "")?;
                done(from)
            } else {
                Transfer::Pending {
                from: from.to_string(),
                detail: "the transfer was requested earlier and is not at the new owner yet; not requested again".into(),
                new_repository_id: None,
            }
            },
        );
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
        200 if taken.body["id"].as_u64() == Some(id) => return Ok(done(from)),
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
                "POST /repos/{from}/transfer new_owner={wanted_owner}{} through the write gate (one per run, at least three minutes apart)",
                new_name.map(|n| format!(" new_name={n}")).unwrap_or_default()
            )],
        });
    }
    io.note(
        "github-transfer",
        &id.to_string(),
        &crate::native::http::now()?.to_string(),
    )?;
    let response = match writing(github_call(
        io,
        dest,
        "POST",
        &format!("/repos/{from}/transfer"),
        Some(match new_name {
            Some(name) => json!({"new_owner": wanted_owner, "new_name": name}),
            None => json!({"new_owner": wanted_owner}),
        }),
        true,
    ))? {
        Ok(response) => response,
        Err(reason) => {
            io.note("github-transfer-clear", &id.to_string(), "")?;
            return blocked(from, reason);
        }
    };
    match response.status {
        200..=202 => {}
        status @ (404 | 422) => {
            io.note("github-transfer-clear", &id.to_string(), "")?;
            return blocked(from, format!("GitHub refused the transfer (HTTP {status})"));
        }
        other => {
            return Err(failure(format!(
                "GitHub transfer failed with HTTP {other}; inspect the repository before retrying"
            )))
        }
    }
    // Read it back by id: only the live name proves the move.
    let live = expect(
        github_call(io, dest, "GET", &format!("/repositories/{id}"), None, false)?,
        &[200],
    )?;
    if live["full_name"]
        .as_str()
        .is_some_and(|n| same(n, &dest.path))
    {
        io.resolve(&format!("{}/create-repo", dest.endpoint.origin))?;
        io.note("github-transfer-clear", &id.to_string(), "")?;
        Ok(done(from))
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

/// `workspace` is the one whose repository the request addresses: the rate and plan windows are recorded per workspace, the
/// old repository's calls belong to the old one.
fn bitbucket_call(
    io: &mut dyn Transport,
    dest: &Destination,
    workspace: &str,
    method: &'static str,
    path: String,
    body: Option<Value>,
    creation: bool,
) -> Result<Response> {
    if dest.provider != Provider::Bitbucket || dest.endpoint.origin != "https://api.bitbucket.org" {
        return Err(failure("Bitbucket Cloud API origin required"));
    }
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

/// A repository addressed by its immutable uuid under a workspace (slugs move; cbkt addresses repositories this way too).
fn by_uuid(workspace: &str, uuid: &str) -> String {
    format!("{workspace}/{}", encode(uuid))
}

/// Every branch and tag of the repository at `address` (`<workspace>/<slug or uuid>`) as kind:name to commit hash, following
/// the pagination. A page without a `values` array is an error, never an empty listing.
pub fn bitbucket_refs(
    io: &mut dyn Transport,
    dest: &Destination,
    address: &str,
) -> Result<BTreeMap<String, String>> {
    let workspace = address.split('/').next().unwrap_or_default().to_string();
    let mut refs = BTreeMap::new();
    let mut next = Some(format!("/2.0/repositories/{address}/refs?pagelen=100"));
    let mut pages = 0;
    while let Some(path) = next.take() {
        pages += 1;
        if pages > 200 {
            return Err(failure("Bitbucket ref listing does not end"));
        }
        let page = expect(
            bitbucket_call(io, dest, &workspace, "GET", path, None, false)?,
            &[200],
        )?;
        let values = page["values"].as_array().ok_or_else(|| {
            failure("Bitbucket ref listing has no values; refusing to read it as empty")
        })?;
        for value in values {
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

/// What the move needs besides the destination (`dest.path` is the new place).
pub struct BitbucketMove<'a> {
    /// The workspace the old repository sits in; it is addressed by `old_uuid` under it.
    pub old_workspace: &'a str,
    pub old_uuid: &'a str,
    /// The Bitbucket project the new repository goes into (the creation destination's namespace).
    pub project_key: &'a str,
    pub private: bool,
    /// Whether this operation may create a repository at all (a rename never does).
    pub allow_create: bool,
    /// The uuid cfrg's state recorded for the new repository: it is accepted by that uuid whatever its description says.
    pub known_new: Option<&'a str>,
}

fn is_ours(target: &Value, marker: &str, known_new: Option<&str>) -> bool {
    target["description"].as_str() == Some(marker)
        || known_new.is_some_and(|known| target["uuid"].as_str() == Some(known))
}

/// One step of the Bitbucket move: create the target, wait for the mirror, delete the old repository after an exact match.
pub fn bitbucket(
    io: &mut dyn Transport,
    dest: &Destination,
    mv: &BitbucketMove,
    apply: bool,
) -> Result<Transfer> {
    let (wanted_ws, wanted_slug) = split(&dest.path)?;
    let old_ws = mv.old_workspace;
    let old_address = by_uuid(old_ws, mv.old_uuid);
    let marker = bitbucket_marker(mv.old_uuid);
    let read_old = |io: &mut dyn Transport| {
        bitbucket_call(
            io,
            dest,
            old_ws,
            "GET",
            format!("/2.0/repositories/{old_address}"),
            None,
            false,
        )
    };
    let target = bitbucket_call(
        io,
        dest,
        wanted_ws,
        "GET",
        format!("/2.0/repositories/{}", dest.path),
        None,
        false,
    )?;
    let old = read_old(io)?;
    let from = match old.status {
        200 => {
            if old.body["uuid"].as_str() != Some(mv.old_uuid) {
                return Err(failure("Bitbucket destination identity mismatch"));
            }
            old.body["full_name"]
                .as_str()
                .ok_or_else(|| failure("Missing Bitbucket repository name"))?
                .to_string()
        }
        404 => {
            // The old repository is gone: resume only if the repository at the new path is the one this move made.
            return match target.status {
                200 if is_ours(&target.body, &marker, mv.known_new) => Ok(Transfer::Done {
                    from: format!("{old_ws}/{wanted_slug}"),
                    detail: json!({"repository_id": target.body["uuid"], "resumed": true}),
                }),
                _ => Err(failure(
                    "Declared Bitbucket repo disappeared and no repository made by its move sits at the new path; refusing replacement",
                )),
            };
        }
        other => {
            return Err(failure(format!(
                "Bitbucket lookup failed with HTTP {other}"
            )))
        }
    };
    // A new repository is created at the wanted workspace AND name, so a different name needs no rename afterwards.
    let (live_ws, _) = split(&from)?;
    if same(live_ws, wanted_ws) {
        return Err(failure("Bitbucket move asked inside one workspace"));
    }
    if mv.project_key.is_empty() {
        return blocked(
            &from,
            "the destination names no Bitbucket project key (namespace)",
        );
    }
    let new_uuid = match target.status {
        404 => {
            if !apply {
                return Ok(Transfer::Planned {
                    from: from.clone(),
                    steps: vec![
                        format!(
                            "create {} in project {} ({}), marked {marker}",
                            dest.path,
                            mv.project_key,
                            if mv.private { "private" } else { "public" }
                        ),
                        "mirror it from Forgejo (a second owned mirror; the old one keeps running)"
                            .into(),
                        format!("compare every branch and tag of {from} and {}", dest.path),
                        format!(
                            "delete {from} only after the compare matches and the old one has refs"
                        ),
                    ],
                });
            }
            if !mv.allow_create {
                return blocked(
                    &from,
                    "this operation never creates a repository; run a reconcile",
                );
            }
            let created = match writing(bitbucket_call(
                io,
                dest,
                wanted_ws,
                "POST",
                format!("/2.0/repositories/{}", dest.path),
                Some(json!({
                    "scm": "git",
                    "is_private": mv.private,
                    "project": {"key": mv.project_key},
                    "description": marker,
                })),
                true,
            ))? {
                Ok(response) => response,
                Err(reason) => return blocked(&from, reason),
            };
            let body = expect(created, &[200, 201])?;
            let uuid = body["uuid"].as_str().map(String::from);
            // Read back before the intent is cleared: only the live repository proves the creation.
            let back = bitbucket_call(
                io,
                dest,
                wanted_ws,
                "GET",
                format!("/2.0/repositories/{}", dest.path),
                None,
                false,
            )?;
            if back.status == 200 {
                io.resolve(&format!("{}/workspace/{wanted_ws}", dest.endpoint.origin))?;
            }
            return Ok(Transfer::Pending {
                from,
                detail: "created the new repository; the mirror must carry every ref before the old one goes".into(),
                new_repository_id: uuid,
            });
        }
        200 => {
            if !is_ours(&target.body, &marker, mv.known_new) {
                return blocked(
                    &from,
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
    let old_refs = bitbucket_refs(io, dest, &old_address)?;
    if old_refs.is_empty() {
        return blocked(
            &from,
            "the old repository has no refs to compare; it is never deleted on an empty compare",
        );
    }
    let new_refs = bitbucket_refs(io, dest, &dest.path)?;
    let findings = compare_refs(&old_refs, &new_refs);
    if !findings.is_empty() {
        let first: Vec<_> = findings.iter().take(5).cloned().collect();
        return Ok(Transfer::Pending {
            detail: format!(
                "{} of {} refs do not match yet ({}); the old repository stays",
                findings.len(),
                old_refs.len(),
                first.join("; ")
            ),
            from,
            new_repository_id: new_uuid,
        });
    }
    if !apply {
        return Ok(Transfer::Planned {
            steps: vec![format!(
                "all {} refs match; delete {from} (the old repository)",
                old_refs.len()
            )],
            from,
        });
    }
    // Right before the delete: it is still the repository we compared.
    let again = read_old(io)?;
    if again.status != 200
        || again.body["uuid"].as_str() != Some(mv.old_uuid)
        || again.body["full_name"].as_str() != Some(from.as_str())
    {
        return Err(failure(
            "The old Bitbucket repository changed during the compare; nothing was deleted",
        ));
    }
    let deleted = match writing(bitbucket_call(
        io,
        dest,
        old_ws,
        "DELETE",
        format!("/2.0/repositories/{old_address}"),
        None,
        false,
    ))? {
        Ok(response) => response,
        Err(reason) => return blocked(&from, reason),
    };
    expect(deleted, &[200, 202, 204])?;
    if read_old(io)?.status != 404 {
        return Err(failure(
            "The old Bitbucket repository still answers after the delete; inspect it",
        ));
    }
    io.resolve(&format!("{}/workspace/{old_ws}", dest.endpoint.origin))?;
    Ok(Transfer::Done {
        from,
        detail: json!({"repository_id": new_uuid, "refs_compared": old_refs.len()}),
    })
}

#[cfg(test)]
mod tests;
