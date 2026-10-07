//! Bitbucket destination operations. A workspace 402 holds every later write.
use cfrg::{
    native::{
        encode,
        http::{expect, Auth, Request, Response, Transport},
        plan_move, Destination, Move, Naming, Provider, Repository,
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

/// The destination addressed by its immutable UUID instead of its name, which a
/// rename makes outdated. Bitbucket answers the UUID under the workspace.
fn by_uuid(dest: &Destination, uuid: &str) -> Result<Destination> {
    let workspace = dest.path.split('/').next().ok_or("Missing workspace")?;
    let mut copy = dest.clone();
    copy.path = format!("{workspace}/{}", encode(uuid));
    Ok(copy)
}

/// Compare the repository's actual name, found by its UUID, with the wanted one
/// (`dest.path`) and rename it when only the slug differs: Bitbucket derives the
/// slug from the name, and the old slug stops answering (no redirect), which is
/// why every lookup goes by UUID. A taken name or another workspace is
/// reported and never forced. Without a recorded UUID the by-name flow runs.
pub fn naming(io: &mut dyn Transport, dest: &Destination, apply: bool) -> Result<Naming> {
    let Some(uuid) = dest.repository_id.as_deref() else {
        return Ok(Naming::Current);
    };
    let known = by_uuid(dest, uuid)?;
    let response = call(io, &known, "GET", "", None, false)?;
    if response.status == 404 {
        return Err("Declared Bitbucket repo disappeared; refusing replacement".into());
    }
    let repository = expect(response, &[200])?;
    if repository["uuid"] != uuid {
        return Err("Bitbucket destination identity mismatch".into());
    }
    let live = repository["full_name"]
        .as_str()
        .ok_or("Missing Bitbucket repository name")?
        .to_string();
    let leaf = match plan_move(&live, &dest.path) {
        Move::Same => return Ok(Naming::Current),
        Move::Transfer => {
            return Ok(Naming::Blocked {
                from: live,
                reason: "the workspace differs; transfers are not supported",
            })
        }
        Move::Rename { leaf } => leaf,
    };
    let wanted = call(io, dest, "GET", "", None, false)?;
    match wanted.status {
        404 => {}
        200 if wanted.body["uuid"] == uuid => return Ok(Naming::Current),
        200 => {
            return Ok(Naming::Blocked {
                from: live,
                reason: "the wanted name is already taken",
            })
        }
        other => return Err(format!("Bitbucket name lookup failed with HTTP {other}").into()),
    }
    if !apply {
        return Ok(Naming::Planned { from: live });
    }
    let response = call(io, &known, "PUT", "", Some(json!({"name":leaf})), false)?;
    match response.status {
        200 if response.body["full_name"] == dest.path => Ok(Naming::Renamed { from: live }),
        200 => Err("Bitbucket rename not confirmed; inspect the repository before retrying".into()),
        400 | 404 | 409 | 422 => Ok(Naming::Blocked {
            from: live,
            reason: "Bitbucket refused the new name",
        }),
        other => Err(format!("Bitbucket rename failed with HTTP {other}").into()),
    }
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
    /// Plays back canned answers and records what was asked.
    struct Script {
        answers: Vec<(u16, Value)>,
        asked: Vec<(&'static str, String, Option<Value>)>,
    }
    impl Script {
        fn new(answers: Vec<(u16, Value)>) -> Self {
            Self {
                answers,
                asked: Vec::new(),
            }
        }
    }
    impl Transport for Script {
        fn send(&mut self, request: Request) -> Result<Response> {
            self.asked
                .push((request.method, request.path, request.body));
            let (status, body) = self.answers.remove(0);
            Ok(Response { status, body })
        }
    }
    fn wanted(path: &str, id: Option<&str>) -> Destination {
        serde_json::from_value(json!({"provider":"bitbucket","endpoint":{"origin":"https://api.bitbucket.org","token_env":"TOKEN"},"path":path,"namespace":"CORE","repository_id":id,"mirror_user":"{m}","password_env":"TOKEN","interval_seconds":3600})).unwrap()
    }
    fn repository(full_name: &str) -> Value {
        json!({"uuid":"{u-1}","full_name":full_name})
    }

    #[test]
    fn a_renamed_primary_renames_the_repository_found_by_uuid() {
        let mut io = Script::new(vec![
            (200, repository("team/old")),
            (404, Value::Null),
            (200, repository("team/new")),
        ]);
        let result = naming(&mut io, &wanted("team/new", Some("{u-1}")), true).unwrap();
        assert_eq!(
            result,
            Naming::Renamed {
                from: "team/old".into()
            }
        );
        let asked: Vec<_> = io.asked.iter().map(|(m, p, _)| (*m, p.as_str())).collect();
        assert_eq!(
            asked,
            [
                ("GET", "/2.0/repositories/team/%7Bu-1%7D"),
                ("GET", "/2.0/repositories/team/new"),
                ("PUT", "/2.0/repositories/team/%7Bu-1%7D"),
            ]
        );
        assert_eq!(io.asked[2].2, Some(json!({"name":"new"})));
    }

    #[test]
    fn planning_writes_nothing_and_a_taken_name_is_not_forced() {
        let mut io = Script::new(vec![(200, repository("team/old")), (404, Value::Null)]);
        let result = naming(&mut io, &wanted("team/new", Some("{u-1}")), false).unwrap();
        assert_eq!(
            result,
            Naming::Planned {
                from: "team/old".into()
            }
        );
        assert!(io.asked.iter().all(|(method, _, _)| *method == "GET"));

        let mut io = Script::new(vec![
            (200, repository("team/old")),
            (200, json!({"uuid":"{other}","full_name":"team/new"})),
        ]);
        let result = naming(&mut io, &wanted("team/new", Some("{u-1}")), true).unwrap();
        assert!(matches!(result, Naming::Blocked { reason, .. } if reason.contains("taken")));
        assert_eq!(io.asked.len(), 2);
    }

    #[test]
    fn another_workspace_a_refusal_the_right_name_or_no_uuid() {
        let mut io = Script::new(vec![(200, repository("elsewhere/old"))]);
        let result = naming(&mut io, &wanted("team/new", Some("{u-1}")), true).unwrap();
        assert!(matches!(result, Naming::Blocked { reason, .. } if reason.contains("workspace")));

        let mut io = Script::new(vec![
            (200, repository("team/old")),
            (404, Value::Null),
            (400, Value::Null),
        ]);
        let result = naming(&mut io, &wanted("team/new", Some("{u-1}")), true).unwrap();
        assert!(matches!(result, Naming::Blocked { reason, .. } if reason.contains("refused")));

        let mut io = Script::new(vec![(200, repository("team/new"))]);
        assert_eq!(
            naming(&mut io, &wanted("team/new", Some("{u-1}")), true).unwrap(),
            Naming::Current
        );
        let mut io = Script::new(vec![]);
        assert_eq!(
            naming(&mut io, &wanted("team/new", None), true).unwrap(),
            Naming::Current
        );
        assert!(io.asked.is_empty());
        let mut io = Script::new(vec![(404, Value::Null)]);
        assert!(naming(&mut io, &wanted("team/new", Some("{u-1}")), true).is_err());
    }
}
