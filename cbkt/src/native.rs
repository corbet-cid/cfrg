//! Bitbucket destination operations. A workspace 402 holds every later write.
use cfrg::{
    native::{
        http::{expect, Auth, Request, Response, Transport},
        Destination, Provider, Repository,
    },
    Result,
};
use serde_json::{json, Value};

pub(crate) fn call(
    io: &mut dyn Transport,
    dest: &Destination,
    method: &'static str,
    suffix: &str,
    body: Option<Value>,
    creation: bool,
) -> Result<Response> {
    if dest.provider != Provider::Bitbucket || dest.endpoint.origin != "https://api.bitbucket.org" {
        return Err("Bitbucket Cloud API origin required".into());
    }
    let workspace = dest.path.split('/').next().ok_or("Missing workspace")?;
    io.send(Request {
        endpoint: dest.endpoint.clone(),
        auth: Auth::Bearer,
        method,
        path: format!("/2.0/repositories/{}{suffix}", dest.path),
        body,
        scope: format!("{}/workspace/{workspace}", dest.endpoint.origin),
        creation,
    })
}
pub fn clone_url(dest: &Destination) -> String {
    format!("https://bitbucket.org/{}.git", dest.path)
}

fn verify(value: Value, repo: &Repository, dest: &Destination) -> Result<Value> {
    if value["full_name"] != dest.path
        || value["is_private"] != repo.private
        || value["scm"] != "git"
        || value["uuid"].as_str().is_none()
        || dest
            .repository_id
            .as_ref()
            .is_some_and(|id| value["uuid"] != *id)
    {
        return Err("Bitbucket destination identity or visibility mismatch".into());
    }
    Ok(value)
}

pub fn ensure_destination_repo(
    io: &mut dyn Transport,
    repo: &Repository,
    dest: &Destination,
    apply: bool,
    allow_create: bool,
) -> Result<Option<Value>> {
    if dest.path.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err("Bitbucket exact-case name unsupported".into());
    }
    let response = call(io, dest, "GET", "", None, false)?;
    if response.status == 200 {
        return Ok(Some(verify(response.body, repo, dest)?));
    }
    if response.status != 404 {
        expect(response, &[200])?;
    }
    if dest.repository_id.is_some() {
        return Err("Declared Bitbucket repo disappeared; refusing replacement".into());
    }
    if !apply || !allow_create {
        return Ok(None);
    }
    let value = expect(
        call(
            io,
            dest,
            "POST",
            "",
            Some(json!({
                "scm":"git", "is_private":repo.private, "project":{"key":dest.namespace},
                "name":dest.path.rsplit('/').next().ok_or("Missing Bitbucket name")?
            })),
            true,
        )?,
        &[200, 201],
    )?;
    Ok(Some(verify(value, repo, dest)?))
}

pub(crate) fn rules(io: &mut dyn Transport, dest: &Destination) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    for page in 1..=20 {
        let value = expect(
            call(
                io,
                dest,
                "GET",
                &format!("/branch-restrictions?pagelen=100&page={page}"),
                None,
                false,
            )?,
            &[200],
        )?;
        let values = value["values"]
            .as_array()
            .ok_or("Malformed Bitbucket restrictions")?;
        for rule in values {
            if result.iter().any(|old: &Value| old["id"] == rule["id"]) {
                return Err("Repeated Bitbucket restrictions page".into());
            }
            result.push(rule.clone());
        }
        if value["next"].is_null() {
            return Ok(result);
        }
        // Never follow a returned URL: increment only our own bounded endpoint.
    }
    Err("Bitbucket restrictions pagination exceeded".into())
}

pub fn exclusive_rules(values: &[Value], user: &str) -> bool {
    exclusive_for(values, &[user])
}

/// Exactly two rules over every branch: pushes only for `users` (nobody when
/// empty), merges for nobody.
pub fn exclusive_for(values: &[Value], users: &[&str]) -> bool {
    if values.len() != 2 {
        return false;
    }
    ["push", "restrict_merges"].iter().all(|kind| {
        values.iter().any(|r| {
            r["kind"] == *kind
                && r["branch_match_kind"] == "glob"
                && r["pattern"] == "*"
                && r["groups"].as_array().is_some_and(Vec::is_empty)
                && r["users"].as_array().is_some_and(|a| {
                    if *kind == "push" {
                        a.len() == users.len()
                            && users.iter().all(|u| a.iter().any(|x| x["uuid"] == *u))
                    } else {
                        a.is_empty()
                    }
                })
        })
    }) && values.iter().all(|r| {
        matches!(r["kind"].as_str(), Some("push" | "restrict_merges")) && r["pattern"] == "*"
    })
}

pub fn protect(io: &mut dyn Transport, dest: &Destination, apply: bool) -> Result<bool> {
    restrict(io, dest, apply, &[dest.mirror_user.as_str()])
}

/// Lock every branch: pushes only for `users` (nobody when empty), merges for
/// nobody. Pipelines are switched off first.
pub fn restrict(
    io: &mut dyn Transport,
    dest: &Destination,
    apply: bool,
    users: &[&str],
) -> Result<bool> {
    // A repository without a single commit has no pipelines configuration yet
    // (404): there is nothing to switch off.
    let response = call(io, dest, "GET", "/pipelines_config", None, false)?;
    let pipelines = if response.status == 404 {
        json!({"enabled": false})
    } else {
        expect(response, &[200])?
    };
    if pipelines["enabled"] != false {
        if !apply {
            return Ok(false);
        }
        let saved = expect(
            call(
                io,
                dest,
                "PUT",
                "/pipelines_config",
                Some(json!({"enabled":false})),
                false,
            )?,
            &[200],
        )?;
        if saved["enabled"] != false {
            return Err("Bitbucket pipelines remain enabled".into());
        }
    }
    let current = rules(io, dest)?;
    if exclusive_for(&current, users) {
        return Ok(true);
    }
    if !apply {
        return Ok(false);
    }
    // Unknown/force/delete/overlapping rules require a declared migration,
    // never automatically erase restrictions to make force-push work.
    if current.iter().any(|r| {
        !matches!(r["kind"].as_str(), Some("push" | "restrict_merges")) || r["pattern"] != "*"
    }) {
        return Err("Existing Bitbucket restrictions require explicit migration".into());
    }
    for kind in ["push", "restrict_merges"] {
        let matches: Vec<_> = current.iter().filter(|r| r["kind"] == kind).collect();
        if matches.len() > 1 {
            return Err("Duplicate Bitbucket restriction".into());
        }
        let payload = json!({"kind":kind, "branch_match_kind":"glob", "pattern":"*", "groups":[],
            "users": if kind == "push" { users.iter().map(|u| json!({"uuid":u})).collect() } else { vec![] }});
        let (method, suffix) = match matches.first() {
            Some(rule) => (
                "PUT",
                format!(
                    "/branch-restrictions/{}",
                    rule["id"].as_u64().ok_or("Missing restriction ID")?
                ),
            ),
            None => ("POST", "/branch-restrictions".into()),
        };
        expect(
            call(io, dest, method, &suffix, Some(payload), false)?,
            &[200, 201],
        )?;
    }
    if !exclusive_for(&rules(io, dest)?, users) {
        return Err("Bitbucket exclusive protection verification failed".into());
    }
    Ok(true)
}

pub fn finalize_default_branch(
    io: &mut dyn Transport,
    repo: &Repository,
    dest: &Destination,
    target: &Value,
    apply: bool,
) -> Result<bool> {
    if target["mainbranch"]["name"] == repo.default_branch {
        return Ok(true);
    }
    if !apply {
        return Ok(false);
    }
    let branch = call(
        io,
        dest,
        "GET",
        &format!(
            "/refs/branches/{}",
            cfrg::native::encode(&repo.default_branch)
        ),
        None,
        false,
    )?;
    if branch.status == 404 {
        return Ok(false);
    }
    if expect(branch, &[200])?["name"] != repo.default_branch {
        return Err("Bitbucket branch identity mismatch".into());
    }
    let value = expect(
        call(
            io,
            dest,
            "PUT",
            "",
            Some(json!({"mainbranch":{"name":repo.default_branch}})),
            false,
        )?,
        &[200],
    )?;
    if value["mainbranch"]["name"] != repo.default_branch {
        return Err("Bitbucket default branch update not confirmed".into());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_freeze_is_a_push_rule_without_users_and_a_named_user_still_locks() {
        let push = |users: Value| json!({"kind":"push","branch_match_kind":"glob","pattern":"*","groups":[],"users":users});
        let merge = json!({"kind":"restrict_merges","branch_match_kind":"glob","pattern":"*","groups":[],"users":[]});
        assert!(exclusive_for(&[push(json!([])), merge.clone()], &[]));
        assert!(!exclusive_for(
            &[push(json!([{"uuid":"{a}"}])), merge.clone()],
            &[]
        ));
        assert!(!exclusive_for(&[push(json!([])), merge], &["{a}"]));
    }

    #[test]
    fn mirror_principal_only_and_force_delete_holds() {
        let push = json!({"kind":"push","branch_match_kind":"glob","pattern":"*","groups":[],"users":[{"uuid":"{mirror}"}]});
        let merge = json!({"kind":"restrict_merges","branch_match_kind":"glob","pattern":"*","groups":[],"users":[]});
        assert!(exclusive_rules(&[push.clone(), merge.clone()], "{mirror}"));
        assert!(!exclusive_rules(&[push.clone(), merge.clone()], "{other}"));
        assert!(!exclusive_rules(
            &[push, merge, json!({"kind":"force","pattern":"*"})],
            "{mirror}"
        ));
    }
}
