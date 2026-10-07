//! GitLab destination lifecycle and exclusive mirror-principal protection.
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
    path: String,
    body: Option<Value>,
    creation: bool,
) -> Result<Response> {
    if dest.provider != Provider::Gitlab {
        return Err("GitLab adapter requires a GitLab destination".into());
    }
    io.send(Request {
        endpoint: dest.endpoint.clone(),
        auth: Auth::PrivateToken,
        method,
        path: format!("/api/v4{path}"),
        body,
        scope: format!(
            "{}/{}",
            dest.endpoint.origin,
            if creation { "create-project" } else { "api" }
        ),
        creation,
    })
}
pub fn clone_url(dest: &Destination) -> String {
    if dest.use_ssh {
        format!(
            "ssh://git@{}/{}.git",
            dest.endpoint.origin.trim_start_matches("https://"),
            dest.path
        )
    } else {
        format!("{}/{}.git", dest.endpoint.origin, dest.path)
    }
}

fn verify(value: Value, repo: &Repository, dest: &Destination) -> Result<Value> {
    if value["path_with_namespace"] != dest.path
        || value["visibility"] != if repo.private { "private" } else { "public" }
        || value["id"].as_u64().is_none()
        || dest
            .repository_id
            .as_ref()
            .is_some_and(|id| value["id"].as_u64().map(|n| n.to_string()).as_ref() != Some(id))
    {
        return Err("GitLab destination identity or visibility mismatch".into());
    }
    Ok(value)
}

/// GET first: an existing project never touches the creation-rate scope.
pub fn ensure_destination_repo(
    io: &mut dyn Transport,
    repo: &Repository,
    dest: &Destination,
    apply: bool,
    allow_create: bool,
) -> Result<Option<Value>> {
    if dest
        .path
        .rsplit('/')
        .next()
        .is_none_or(|p| p.starts_with('.'))
    {
        return Err("GitLab leading-dot profile requires projection".into());
    }
    let response = call(
        io,
        dest,
        "GET",
        format!("/projects/{}", encode(&dest.path)),
        None,
        false,
    )?;
    if response.status == 200 {
        let mut value = verify(response.body, repo, dest)?;
        if value["builds_access_level"] != "disabled" && apply {
            value = verify(
                expect(
                    call(
                        io,
                        dest,
                        "PUT",
                        format!("/projects/{}", encode(&dest.path)),
                        Some(json!({"builds_access_level":"disabled"})),
                        false,
                    )?,
                    &[200],
                )?,
                repo,
                dest,
            )?;
            if value["builds_access_level"] != "disabled" {
                return Err("GitLab pipelines remain enabled".into());
            }
        }
        return Ok(Some(value));
    }
    if response.status != 404 {
        expect(response, &[200])?;
    }
    if dest.repository_id.is_some() {
        return Err("Declared GitLab project disappeared; refusing replacement".into());
    }
    if !apply || !allow_create {
        return Ok(None);
    }
    let namespace_id: u64 = dest
        .namespace
        .parse()
        .map_err(|_| "GitLab namespace ID required")?;
    let name = dest.path.rsplit('/').next().ok_or("Missing GitLab name")?;
    let value = expect(
        call(
            io,
            dest,
            "POST",
            "/projects".into(),
            Some(json!({
                "namespace_id":namespace_id, "name":name, "path":name,
                "visibility":if repo.private {"private"} else {"public"},
                "initialize_with_readme":false, "builds_access_level":"disabled"
            })),
            true,
        )?,
        &[201],
    )?;
    Ok(Some(verify(value, repo, dest)?))
}

/// Compare the project's actual name, found by its immutable ID, with the wanted
/// one (`dest.path`) and rename it when only the last component differs. GitLab
/// keeps redirecting the old path, so existing remotes keep working. The path
/// moves, and so does the display name when it carried the same text. A taken
/// name or a different namespace is reported and never forced. Without a
/// recorded ID there is nothing to look up and the caller's by-path flow runs.
pub fn naming(io: &mut dyn Transport, dest: &Destination, apply: bool) -> Result<Naming> {
    let Some(id) = dest.repository_id.as_deref() else {
        return Ok(Naming::Current);
    };
    let id: u64 = id
        .parse()
        .map_err(|_| "GitLab repository_id must be a numeric project ID")?;
    let response = call(io, dest, "GET", format!("/projects/{id}"), None, false)?;
    if response.status == 404 {
        return Err("Declared GitLab project disappeared; refusing replacement".into());
    }
    let project = expect(response, &[200])?;
    if project["id"].as_u64() != Some(id) {
        return Err("GitLab destination identity mismatch".into());
    }
    let live = project["path_with_namespace"]
        .as_str()
        .ok_or("Missing GitLab project path")?
        .to_string();
    let leaf = match plan_move(&live, &dest.path) {
        Move::Same => return Ok(Naming::Current),
        Move::Transfer => {
            return Ok(Naming::Blocked {
                from: live,
                reason: "the namespace differs; transfers are not supported",
            })
        }
        Move::Rename { leaf } => leaf,
    };
    let wanted = call(
        io,
        dest,
        "GET",
        format!("/projects/{}", encode(&dest.path)),
        None,
        false,
    )?;
    match wanted.status {
        // 301: only a redirect that an earlier project of that name left behind.
        404 | 301 => {}
        200 if wanted.body["id"].as_u64() == Some(id) => return Ok(Naming::Current),
        200 => {
            return Ok(Naming::Blocked {
                from: live,
                reason: "the wanted name is already taken",
            })
        }
        other => return Err(format!("GitLab name lookup failed with HTTP {other}").into()),
    }
    if !apply {
        return Ok(Naming::Planned { from: live });
    }
    let mut body = json!({"path":leaf});
    if project["name"] == project["path"] {
        body["name"] = json!(leaf);
    }
    let response = call(
        io,
        dest,
        "PUT",
        format!("/projects/{id}"),
        Some(body),
        false,
    )?;
    match response.status {
        200 if response.body["path_with_namespace"] == dest.path => {
            Ok(Naming::Renamed { from: live })
        }
        200 => Err("GitLab rename not confirmed; inspect the project before retrying".into()),
        400 | 404 | 409 | 422 => Ok(Naming::Blocked {
            from: live,
            reason: "GitLab refused the new name",
        }),
        other => Err(format!("GitLab rename failed with HTTP {other}").into()),
    }
}

/// Remove the deploy key cfrg enrolled for a mirror that no longer exists (its
/// private half died with the mirror), so a replaced mirror leaves no orphan.
pub fn forget_key(io: &mut dyn Transport, dest: &Destination, remote_name: &str) -> Result<()> {
    let title = format!("cfrg:{remote_name}");
    for page in 1..=20 {
        let value = expect(
            call(
                io,
                dest,
                "GET",
                format!(
                    "/projects/{}/deploy_keys?per_page=100&page={page}",
                    encode(&dest.path)
                ),
                None,
                false,
            )?,
            &[200],
        )?;
        let keys = value.as_array().ok_or("Malformed deploy key list")?;
        if let Some(key) = keys.iter().find(|key| key["title"] == title.as_str()) {
            let id = key["id"].as_u64().ok_or("Missing deploy key ID")?;
            expect(
                call(
                    io,
                    dest,
                    "DELETE",
                    format!("/projects/{}/deploy_keys/{id}", encode(&dest.path)),
                    None,
                    false,
                )?,
                &[204],
            )?;
            return Ok(());
        }
        if keys.len() < 100 {
            return Ok(());
        }
    }
    Err("Deploy key pagination bound exceeded".into())
}

/// Strict verification includes every overlapping rule: GitLab uses the most
/// permissive matching rule. We protect all branches, including the default.
pub fn exclusive_rules(values: &[Value], user: u64) -> bool {
    exclusive_principal(values, "user_id", user)
}

fn exclusive_principal(values: &[Value], field: &str, id: u64) -> bool {
    let only = |levels: &Value, push: bool| {
        levels.as_array().is_some_and(|rows| {
            !rows.is_empty()
                && rows.iter().all(|r| {
                    let no_group = r["group_id"].is_null()
                        && r["member_role_id"].is_null()
                        && (field == "deploy_key_id" || r["deploy_key_id"].is_null())
                        && (field == "user_id" || r["user_id"].is_null());
                    no_group
                        && ((r[field].as_u64() == Some(id) && push)
                            || (r["user_id"].is_null()
                                && r["deploy_key_id"].is_null()
                                && r["access_level"] == 0))
                })
        })
    };
    values.iter().any(|v| {
        v["name"] == "*"
            && v["push_access_levels"]
                .as_array()
                .is_some_and(|a| a.iter().any(|r| r[field] == id))
    }) && values.iter().all(|v| {
        v["allow_force_push"] == true
            && only(&v["push_access_levels"], true)
            && only(&v["merge_access_levels"], false)
    })
}

pub(crate) fn rules(io: &mut dyn Transport, dest: &Destination) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    for page in 1..=20 {
        let value = expect(
            call(
                io,
                dest,
                "GET",
                format!(
                    "/projects/{}/protected_branches?per_page=100&page={page}",
                    encode(&dest.path)
                ),
                None,
                false,
            )?,
            &[200],
        )?;
        let values = value
            .as_array()
            .ok_or("Malformed GitLab branch protections")?;
        if values.is_empty() {
            return Ok(result);
        }
        for value in values {
            if result
                .iter()
                .any(|old: &Value| old["name"] == value["name"])
            {
                return Err("Repeated GitLab protection page".into());
            }
            result.push(value.clone());
        }
    }
    Err("GitLab protection pagination exceeded".into())
}

pub fn protect(io: &mut dyn Transport, dest: &Destination, apply: bool) -> Result<bool> {
    let user: u64 = dest
        .mirror_user
        .parse()
        .map_err(|_| "GitLab mirror_user must be numeric user ID")?;
    protect_principal(io, dest, apply, "user_id", user)
}

pub fn finalize_default_branch(
    io: &mut dyn Transport,
    repo: &Repository,
    dest: &Destination,
    target: &Value,
    apply: bool,
) -> Result<bool> {
    if target["default_branch"] == repo.default_branch {
        return Ok(true);
    }
    if !apply {
        return Ok(false);
    }
    let branch = call(
        io,
        dest,
        "GET",
        format!(
            "/projects/{}/repository/branches/{}",
            encode(&dest.path),
            encode(&repo.default_branch)
        ),
        None,
        false,
    )?;
    if branch.status == 404 {
        return Ok(false);
    }
    if expect(branch, &[200])?["name"] != repo.default_branch {
        return Err("GitLab branch identity mismatch".into());
    }
    let value = expect(
        call(
            io,
            dest,
            "PUT",
            format!("/projects/{}", encode(&dest.path)),
            Some(json!({"default_branch":repo.default_branch})),
            false,
        )?,
        &[200],
    )?;
    if value["default_branch"] != repo.default_branch {
        return Err("GitLab default branch update not confirmed".into());
    }
    Ok(true)
}

fn protect_principal(
    io: &mut dyn Transport,
    dest: &Destination,
    apply: bool,
    field: &str,
    id: u64,
) -> Result<bool> {
    let current = rules(io, dest)?;
    if exclusive_principal(&current, field, id) {
        return Ok(true);
    }
    if !apply {
        return Ok(false);
    }
    // Updating existing protection is possible without an unprotected interval.
    // Never delete protection to make a write succeed, and never widen to a role.
    for rule in &current {
        let name = rule["name"]
            .as_str()
            .ok_or("Missing protected branch name")?;
        let push = access_delta(&rule["push_access_levels"], field, id)?;
        let merge = access_delta(&rule["merge_access_levels"], "access_level", 0)?;
        if push.is_empty() && merge.is_empty() && rule["allow_force_push"] == true {
            continue;
        }
        let mut body = json!({"allow_force_push":true});
        if !push.is_empty() {
            body["allowed_to_push"] = json!(push);
        }
        if !merge.is_empty() {
            body["allowed_to_merge"] = json!(merge);
        }
        expect(
            call(
                io,
                dest,
                "PATCH",
                format!(
                    "/projects/{}/protected_branches/{}",
                    encode(&dest.path),
                    encode(name)
                ),
                Some(body),
                false,
            )?,
            &[200],
        )?;
    }
    if !current.iter().any(|r| r["name"] == "*") {
        let mut body = json!({"name":"*", "allow_force_push":true, "push_access_level":0, "merge_access_level":0});
        if field != "access_level" {
            body["allowed_to_push"] = json!([{field:id}]);
        }
        expect(
            call(
                io,
                dest,
                "POST",
                format!("/projects/{}/protected_branches", encode(&dest.path)),
                Some(body),
                false,
            )?,
            &[201],
        )?;
    }
    if !exclusive_principal(&rules(io, dest)?, field, id) {
        return Err(
            "GitLab did not enforce exclusive mirror user (tier capability); no mirror enabled"
                .into(),
        );
    }
    Ok(true)
}

fn access_delta(levels: &Value, field: &str, id: u64) -> Result<Vec<Value>> {
    let mut changes = Vec::new();
    let mut kept = false;
    for row in levels
        .as_array()
        .ok_or("Missing protected branch access levels")?
    {
        let matches = row[field] == id
            && ["user_id", "group_id", "member_role_id", "deploy_key_id"]
                .iter()
                .all(|key| *key == field || row[*key].is_null());
        if matches && !kept {
            kept = true;
        } else {
            changes.push(
                json!({"id":row["id"].as_u64().ok_or("Missing access level ID")?, "_destroy":true}),
            );
        }
    }
    if !kept {
        changes.push(json!({field:id}));
    }
    Ok(changes)
}

/// Before the new Forgejo key has write access, deny all branch writers.
pub fn deny_branch_writes(io: &mut dyn Transport, dest: &Destination) -> Result<()> {
    protect_principal(io, dest, true, "access_level", 0)?;
    Ok(())
}

/// Generated keys stay inside Forgejo. Only their public part crosses to GitLab.
/// The creating API principal must be the declared mirror account.
pub fn protect_ssh(
    io: &mut dyn Transport,
    dest: &Destination,
    mirror: &Value,
    apply: bool,
) -> Result<bool> {
    let public_key = mirror["public_key"]
        .as_str()
        .filter(|s| s.starts_with("ssh-"))
        .ok_or("Forgejo did not expose an SSH public key")?;
    let key_parts = |s: &str| {
        s.split_whitespace()
            .take(2)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let mut found = None;
    for page in 1..=20 {
        let value = expect(
            call(
                io,
                dest,
                "GET",
                format!(
                    "/projects/{}/deploy_keys?per_page=100&page={page}",
                    encode(&dest.path)
                ),
                None,
                false,
            )?,
            &[200],
        )?;
        let values = value.as_array().ok_or("Malformed deploy key list")?;
        if values.is_empty() {
            break;
        }
        for key in values {
            if key["key"]
                .as_str()
                .is_some_and(|s| key_parts(s) == key_parts(public_key))
            {
                if found.is_some() {
                    return Err("Duplicate/repeated deploy key".into());
                }
                found = Some(key.clone());
            }
        }
        if page == 20 {
            return Err("Deploy key pagination bound exceeded".into());
        }
    }
    if found.is_none() {
        if !apply {
            return Ok(false);
        }
        deny_branch_writes(io, dest)?;
        let user = expect(call(io, dest, "GET", "/user".into(), None, false)?, &[200])?;
        if user["id"].as_u64().map(|v| v.to_string()).as_deref() != Some(dest.mirror_user.as_str())
        {
            return Err("GitLab API credential is not the declared mirror principal".into());
        }
        let title = format!(
            "cfrg:{}",
            mirror["remote_name"]
                .as_str()
                .ok_or("Missing SSH mirror identity")?
        );
        found = Some(expect(
            call(
                io,
                dest,
                "POST",
                format!("/projects/{}/deploy_keys", encode(&dest.path)),
                Some(json!({"title":title, "key":public_key, "can_push":true})),
                false,
            )?,
            &[201],
        )?);
    }
    let key = found.ok_or("Missing deploy key")?;
    let id = key["id"].as_u64().ok_or("Missing deploy key ID")?;
    if key["can_push"] != true
        || key["key"]
            .as_str()
            .is_none_or(|s| key_parts(s) != key_parts(public_key))
    {
        return Err("Deploy key capability or identity mismatch".into());
    }
    protect_principal(io, dest, apply, "deploy_key_id", id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_delta_preserves_matching_grants_without_duplicate_recreation() {
        let no_access = json!([{"id":123,"access_level":0}]);
        assert!(access_delta(&no_access, "access_level", 0)
            .unwrap()
            .is_empty());
        assert_eq!(
            access_delta(&no_access, "deploy_key_id", 7).unwrap(),
            vec![
                json!({"id":123,"_destroy":true}),
                json!({"deploy_key_id":7})
            ]
        );
        let key = json!([{"id":124,"access_level":40,"deploy_key_id":7}]);
        assert!(access_delta(&key, "deploy_key_id", 7).unwrap().is_empty());
    }
    #[test]
    fn permissive_overlapping_rule_prevents_enrollment() {
        let exact = json!({"name":"*", "allow_force_push":true, "push_access_levels":[{"user_id":7}], "merge_access_levels":[{"access_level":0}]});
        assert!(exclusive_rules(std::slice::from_ref(&exact), 7));
        let broad = json!({"name":"main", "allow_force_push":true, "push_access_levels":[{"access_level":40}], "merge_access_levels":[{"access_level":0}]});
        assert!(!exclusive_rules(&[exact, broad], 7));
    }

    #[test]
    fn deploy_key_and_deny_all_are_exclusive_without_role_fallback() {
        let key = json!({"name":"*", "allow_force_push":true, "push_access_levels":[{"deploy_key_id":9,"access_level":40}], "merge_access_levels":[{"access_level":0}]});
        assert!(exclusive_principal(
            std::slice::from_ref(&key),
            "deploy_key_id",
            9
        ));
        assert!(!exclusive_rules(&[key], 9));
        let deny = json!({"name":"*", "allow_force_push":true, "push_access_levels":[{"access_level":0}], "merge_access_levels":[{"access_level":0}]});
        assert!(exclusive_principal(&[deny], "access_level", 0));
    }

    struct Existing;
    struct Missing;
    impl Transport for Missing {
        fn send(&mut self, request: Request) -> Result<Response> {
            assert_eq!(request.method, "GET");
            assert!(!request.creation);
            Ok(Response {
                status: 404,
                body: Value::Null,
            })
        }
    }
    impl Transport for Existing {
        fn send(&mut self, request: Request) -> Result<Response> {
            assert_eq!(request.method, "GET");
            assert!(!request.creation);
            assert!(request.scope.ends_with("/api"));
            assert_eq!(request.path, "/api/v4/projects/team%2Frepo");
            Ok(Response {
                status: 200,
                body: json!({"id":7,"path_with_namespace":"team/repo","visibility":"private","builds_access_level":"disabled"}),
            })
        }
    }
    #[test]
    fn existing_destination_never_uses_project_creation_scope() {
        let repo: Repository = serde_json::from_value(json!({"path":"team/repo","source_id":1,"private":true,"default_branch":"main","content":"native-git","destinations":[]})).unwrap();
        let dest: Destination = serde_json::from_value(json!({"provider":"gitlab","endpoint":{"origin":"https://gitlab.example","token_env":"TOKEN"},"path":"team/repo","namespace":"2","repository_id":"7","mirror_user":"9","password_env":"TOKEN","interval_seconds":3600})).unwrap();
        assert!(
            ensure_destination_repo(&mut Existing, &repo, &dest, true, false)
                .unwrap()
                .is_some()
        );
        let mut absent = dest;
        absent.repository_id = None;
        assert!(
            ensure_destination_repo(&mut Missing, &repo, &absent, true, false)
                .unwrap()
                .is_none()
        );
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
        serde_json::from_value(json!({"provider":"gitlab","endpoint":{"origin":"https://gitlab.example","token_env":"TOKEN"},"path":path,"namespace":"2","repository_id":id,"mirror_user":"9","password_env":"TOKEN","interval_seconds":3600})).unwrap()
    }
    fn project(path: &str, name: &str) -> Value {
        json!({"id":7,"path_with_namespace":path,"name":name,"path":path.rsplit('/').next().unwrap()})
    }

    #[test]
    fn a_renamed_primary_renames_the_project_inside_its_namespace() {
        let mut io = Script::new(vec![
            (200, project("team/old", "old")),
            (404, Value::Null),
            (200, json!({"path_with_namespace":"team/new"})),
        ]);
        let result = naming(&mut io, &wanted("team/new", Some("7")), true).unwrap();
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
                ("GET", "/api/v4/projects/7"),
                ("GET", "/api/v4/projects/team%2Fnew"),
                ("PUT", "/api/v4/projects/7"),
            ]
        );
        // The display name moved with the path because it carried the same text.
        assert_eq!(io.asked[2].2, Some(json!({"path":"new","name":"new"})));
    }

    #[test]
    fn a_custom_display_name_is_left_alone() {
        let mut io = Script::new(vec![
            (200, project("team/old", "Old, with a story")),
            (404, Value::Null),
            (200, json!({"path_with_namespace":"team/new"})),
        ]);
        naming(&mut io, &wanted("team/new", Some("7")), true).unwrap();
        assert_eq!(io.asked[2].2, Some(json!({"path":"new"})));
    }

    #[test]
    fn planning_writes_nothing() {
        let mut io = Script::new(vec![(200, project("team/old", "old")), (404, Value::Null)]);
        let result = naming(&mut io, &wanted("team/new", Some("7")), false).unwrap();
        assert_eq!(
            result,
            Naming::Planned {
                from: "team/old".into()
            }
        );
        assert!(io.asked.iter().all(|(method, _, _)| *method == "GET"));
    }

    #[test]
    fn a_taken_name_or_another_namespace_is_reported_not_forced() {
        let mut io = Script::new(vec![
            (200, project("team/old", "old")),
            (200, json!({"id":99,"path_with_namespace":"team/new"})),
        ]);
        let result = naming(&mut io, &wanted("team/new", Some("7")), true).unwrap();
        assert!(matches!(result, Naming::Blocked { reason, .. } if reason.contains("taken")));
        assert_eq!(io.asked.len(), 2);

        let mut io = Script::new(vec![(200, project("elsewhere/old", "old"))]);
        let result = naming(&mut io, &wanted("team/new", Some("7")), true).unwrap();
        assert!(matches!(result, Naming::Blocked { reason, .. } if reason.contains("namespace")));
        assert_eq!(io.asked.len(), 1);

        let mut io = Script::new(vec![
            (200, project("team/old", "old")),
            (404, Value::Null),
            (422, Value::Null),
        ]);
        let result = naming(&mut io, &wanted("team/new", Some("7")), true).unwrap();
        assert!(matches!(result, Naming::Blocked { reason, .. } if reason.contains("refused")));
    }

    #[test]
    fn the_right_name_or_no_recorded_identity_asks_for_nothing_more() {
        let mut io = Script::new(vec![(200, project("team/new", "new"))]);
        assert_eq!(
            naming(&mut io, &wanted("team/new", Some("7")), true).unwrap(),
            Naming::Current
        );
        assert_eq!(io.asked.len(), 1);
        let mut io = Script::new(vec![]);
        assert_eq!(
            naming(&mut io, &wanted("team/new", None), true).unwrap(),
            Naming::Current
        );
        assert!(io.asked.is_empty());
        let mut io = Script::new(vec![(404, Value::Null)]);
        assert!(naming(&mut io, &wanted("team/new", Some("7")), true).is_err());
    }

    #[test]
    fn only_the_key_of_the_replaced_mirror_is_removed() {
        let mut io = Script::new(vec![
            (
                200,
                json!([{"id":1,"title":"cfrg:remote_mirror_old"},{"id":2,"title":"cfrg:remote_mirror_new"}]),
            ),
            (204, Value::Null),
        ]);
        forget_key(&mut io, &wanted("team/new", Some("7")), "remote_mirror_old").unwrap();
        assert_eq!(io.asked[1].0, "DELETE");
        assert_eq!(io.asked[1].1, "/api/v4/projects/team%2Fnew/deploy_keys/1");
        let mut io = Script::new(vec![(
            200,
            json!([{"id":2,"title":"cfrg:remote_mirror_new"}]),
        )]);
        forget_key(&mut io, &wanted("team/new", Some("7")), "remote_mirror_old").unwrap();
        assert_eq!(io.asked.len(), 1);
    }
}
