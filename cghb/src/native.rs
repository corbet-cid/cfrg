//! GitHub as a native push-mirror destination: ensure the repository, lock it
//! as a receiver, verify its heads. Forgejo pushes; this module only prepares
//! and checks the destination. GitHub is a mirror target, never a primary.
//!
//! The receiver lock is one branch ruleset on the default branch, which only an
//! organisation owner bypasses (the mirror principal). Every other branch stays
//! unprotected: a protected ref is also the one a mirror cannot delete, so a
//! branch that the primary removes would otherwise stay behind (GitLab even
//! rejects the whole push). GitHub Free has rulesets for public repositories
//! only; a private destination there cannot be locked and is reported, never
//! mirrored unprotected.
use cfrg::{
    native::{
        encode,
        http::{expect, Auth, Request, Response, Transport},
        Destination, Naming, Provider, Repository,
    },
    Result,
};
use serde_json::{json, Value};

/// Name of the one ruleset cfrg owns on a receiver.
pub const LOCK_NAME: &str = "cfrg receiver lock";

pub(crate) fn call(
    io: &mut dyn Transport,
    dest: &Destination,
    method: &'static str,
    suffix: &str,
    body: Option<Value>,
    creation: bool,
) -> Result<Response> {
    if dest.provider != Provider::Github || dest.endpoint.origin != "https://api.github.com" {
        return Err("GitHub API origin required".into());
    }
    io.send(Request {
        endpoint: dest.endpoint.clone(),
        auth: Auth::Bearer,
        method,
        path: suffix.to_string(),
        body,
        scope: format!(
            "{}/{}",
            dest.endpoint.origin,
            if creation { "create-repo" } else { "api" }
        ),
        creation,
    })
}

pub fn clone_url(dest: &Destination) -> String {
    format!("https://github.com/{}.git", dest.path)
}

fn owner_of(dest: &Destination) -> Result<&str> {
    dest.path
        .split('/')
        .next()
        .filter(|owner| !owner.is_empty())
        .ok_or_else(|| "Missing GitHub owner".into())
}

/// `heads/<branch>` with every segment percent-encoded and the slashes kept.
fn branch_path(branch: &str) -> String {
    branch.split('/').map(encode).collect::<Vec<_>>().join("/")
}

fn verify(value: Value, repo: &Repository, dest: &Destination) -> Result<Value> {
    if !value["full_name"]
        .as_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(&dest.path))
        || value["private"] != repo.private
        || value["id"].as_u64().is_none()
        || value["archived"] == true
        || dest
            .repository_id
            .as_ref()
            .is_some_and(|id| value["id"].as_u64().map(|n| n.to_string()).as_ref() != Some(id))
    {
        return Err("GitHub destination identity, visibility or state mismatch".into());
    }
    Ok(value)
}

/// GET first: an existing repository never touches the creation scope. GitHub
/// Actions is switched off on the destination (a mirror carries workflow files
/// that must not run there).
pub fn ensure_destination_repo(
    io: &mut dyn Transport,
    repo: &Repository,
    dest: &Destination,
    apply: bool,
    allow_create: bool,
) -> Result<Option<Value>> {
    let response = call(
        io,
        dest,
        "GET",
        &format!("/repos/{}", dest.path),
        None,
        false,
    )?;
    let value = if response.status == 200 {
        verify(response.body, repo, dest)?
    } else {
        if response.status != 404 {
            expect(response, &[200])?;
        }
        if dest.repository_id.is_some() {
            return Err("Declared GitHub repository disappeared; refusing replacement".into());
        }
        if !apply || !allow_create {
            return Ok(None);
        }
        let name = dest.path.rsplit('/').next().ok_or("Missing GitHub name")?;
        let created = expect(
            call(
                io,
                dest,
                "POST",
                &format!("/orgs/{}/repos", owner_of(dest)?),
                Some(json!({
                    "name":name, "private":repo.private, "has_issues":false,
                    "has_projects":false, "has_wiki":false, "auto_init":false
                })),
                true,
            )?,
            &[201],
        )?;
        verify(created, repo, dest)?
    };
    disable_actions(io, dest, apply)?;
    Ok(Some(value))
}

fn disable_actions(io: &mut dyn Transport, dest: &Destination, apply: bool) -> Result<()> {
    let permissions = expect(
        call(
            io,
            dest,
            "GET",
            &format!("/repos/{}/actions/permissions", dest.path),
            None,
            false,
        )?,
        &[200],
    )?;
    if permissions["enabled"] == false || !apply {
        return Ok(());
    }
    expect(
        call(
            io,
            dest,
            "PUT",
            &format!("/repos/{}/actions/permissions", dest.path),
            Some(json!({"enabled":false})),
            false,
        )?,
        &[204],
    )?;
    Ok(())
}

/// Compare the repository's actual name, found by its immutable ID, with the
/// wanted one. GitHub renames are never applied here: the old name redirects,
/// and a rename is a deliberate act in GitHub, so it is reported and never
/// forced. Without a recorded ID there is nothing to look up.
pub fn naming(io: &mut dyn Transport, dest: &Destination, _apply: bool) -> Result<Naming> {
    let Some(id) = dest.repository_id.as_deref() else {
        return Ok(Naming::Current);
    };
    let id: u64 = id
        .parse()
        .map_err(|_| "GitHub repository_id must be a numeric repository ID")?;
    let response = call(io, dest, "GET", &format!("/repositories/{id}"), None, false)?;
    if response.status == 404 {
        return Err("Declared GitHub repository disappeared; refusing replacement".into());
    }
    let repository = expect(response, &[200])?;
    if repository["id"].as_u64() != Some(id) {
        return Err("GitHub destination identity mismatch".into());
    }
    let live = repository["full_name"]
        .as_str()
        .ok_or("Missing GitHub repository name")?
        .to_string();
    if live.eq_ignore_ascii_case(&dest.path) {
        return Ok(Naming::Current);
    }
    let same_owner = live
        .split('/')
        .next()
        .zip(dest.path.split('/').next())
        .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b));
    Ok(Naming::Blocked {
        from: live,
        reason: if same_owner {
            "GitHub repositories are not renamed by cfrg; rename it in GitHub or pin the path"
        } else {
            "the owner differs; transfers are not supported"
        },
    })
}

/// The ruleset that locks the default branch. Organisation owners bypass it
/// (actor 1 of type `OrganizationAdmin`): the mirror principal is one.
fn lock_body() -> Value {
    json!({
        "name": LOCK_NAME, "target": "branch", "enforcement": "active",
        "bypass_actors": [{"actor_id":1, "actor_type":"OrganizationAdmin", "bypass_mode":"always"}],
        "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}},
        "rules": [{"type":"deletion"}, {"type":"non_fast_forward"}, {"type":"update"}]
    })
}

/// Whether `ruleset` (as read back) is exactly the lock.
pub fn is_lock(ruleset: &Value) -> bool {
    let names = |value: &Value| -> Option<Vec<String>> {
        value
            .as_array()?
            .iter()
            .map(|row| row.as_str().map(String::from))
            .collect()
    };
    let mut kinds: Vec<&str> = ruleset["rules"]
        .as_array()
        .map(|rules| rules.iter().filter_map(|r| r["type"].as_str()).collect())
        .unwrap_or_default();
    kinds.sort_unstable();
    ruleset["name"] == LOCK_NAME
        && ruleset["target"] == "branch"
        && ruleset["enforcement"] == "active"
        && names(&ruleset["conditions"]["ref_name"]["include"])
            .is_some_and(|include| include == ["~DEFAULT_BRANCH"])
        && names(&ruleset["conditions"]["ref_name"]["exclude"]).is_some_and(|x| x.is_empty())
        && ruleset["bypass_actors"].as_array().is_some_and(|actors| {
            actors.len() == 1
                && actors[0]["actor_type"] == "OrganizationAdmin"
                // Sent as 1, read back as null.
                && (actors[0]["actor_id"].is_null() || actors[0]["actor_id"] == 1)
                && actors[0]["bypass_mode"] == "always"
        })
        && kinds == ["deletion", "non_fast_forward", "update"]
        && ruleset["rules"].as_array().is_some_and(|r| r.len() == 3)
}

/// Lock the receiver: one ruleset on the default branch, nothing on any other
/// branch. Returns false (nothing written) when it is not in place and `apply`
/// is off, or when GitHub's tier cannot lock this repository at all.
pub fn protect(
    io: &mut dyn Transport,
    repo: &Repository,
    dest: &Destination,
    apply: bool,
) -> Result<bool> {
    let owner = owner_of(dest)?;
    if repo.private {
        // Free organisations have no rulesets on private repositories (HTTP 403,
        // which the transport treats as a stop): never call the API to find out.
        let organisation = expect(
            call(io, dest, "GET", &format!("/orgs/{owner}"), None, false)?,
            &[200],
        )?;
        if organisation["plan"]["name"] == "free" {
            return Ok(false);
        }
    }
    // The mirror principal must be able to bypass the lock, or no mirror works.
    let me = expect(call(io, dest, "GET", "/user", None, false)?, &[200])?;
    if !me["login"]
        .as_str()
        .is_some_and(|login| login.eq_ignore_ascii_case(&dest.mirror_user))
    {
        return Err("GitHub API credential is not the declared mirror principal".into());
    }
    let membership = expect(
        call(
            io,
            dest,
            "GET",
            &format!("/orgs/{owner}/memberships/{}", dest.mirror_user),
            None,
            false,
        )?,
        &[200],
    )?;
    if membership["role"] != "admin" || membership["state"] != "active" {
        return Err(
            "GitHub mirror principal is not an organisation owner; it could not bypass the lock; no mirror enabled"
                .into(),
        );
    }
    let list = expect(
        call(
            io,
            dest,
            "GET",
            &format!("/repos/{}/rulesets?per_page=100", dest.path),
            None,
            false,
        )?,
        &[200],
    )?;
    let list = list.as_array().ok_or("Malformed GitHub ruleset list")?;
    let ours: Vec<&Value> = list
        .iter()
        .filter(|r| r["name"] == LOCK_NAME && r["source_type"] == "Repository")
        .collect();
    // Another active branch ruleset (the organisation's too) may forbid deleting a
    // branch: it needs an explicit migration, cfrg never removes it.
    if list.iter().any(|r| {
        r["target"] == "branch"
            && r["enforcement"] == "active"
            && !(r["name"] == LOCK_NAME && r["source_type"] == "Repository")
    }) {
        return Err("Existing GitHub branch rulesets require explicit migration".into());
    }
    if ours.len() > 1 {
        return Err("Duplicate GitHub receiver lock".into());
    }
    let ruleset_path = |id: u64| format!("/repos/{}/rulesets/{id}", dest.path);
    let id = match ours.first() {
        Some(summary) => summary["id"].as_u64().ok_or("Missing GitHub ruleset ID")?,
        None if apply => expect(
            call(
                io,
                dest,
                "POST",
                &format!("/repos/{}/rulesets", dest.path),
                Some(lock_body()),
                false,
            )?,
            &[201],
        )?["id"]
            .as_u64()
            .ok_or("Missing GitHub ruleset ID")?,
        None => return Ok(false),
    };
    // Whatever was there or was just written is read back from GitHub.
    let read = |io: &mut dyn Transport| -> Result<Value> {
        expect(
            call(io, dest, "GET", &ruleset_path(id), None, false)?,
            &[200],
        )
    };
    let mut locked = read(&mut *io)?;
    if !is_lock(&locked) {
        if !apply {
            return Ok(false);
        }
        expect(
            call(io, dest, "PUT", &ruleset_path(id), Some(lock_body()), false)?,
            &[200],
        )?;
        locked = read(&mut *io)?;
    }
    if !is_lock(&locked) {
        return Err(
            "GitHub did not enforce the receiver lock (tier capability); no mirror enabled".into(),
        );
    }
    // GitHub says whether the calling credential is exempt from the ruleset.
    if locked["current_user_can_bypass"] != "always" {
        return Err(
            "GitHub mirror credential cannot bypass the receiver lock; no mirror enabled".into(),
        );
    }
    // No other protected branch: classic protection blocks deletion too.
    let response = call(
        io,
        dest,
        "GET",
        &format!("/repos/{}/branches?protected=true&per_page=100", dest.path),
        None,
        false,
    )?;
    // 409: an empty repository has no branches yet.
    if response.status != 409 {
        let branches = expect(response, &[200])?;
        if branches
            .as_array()
            .ok_or("Malformed GitHub branch list")?
            .iter()
            .any(|b| b["name"] != repo.default_branch)
        {
            return Err(
                "Protected branches beyond the default branch require explicit migration".into(),
            );
        }
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
        &format!(
            "/repos/{}/branches/{}",
            dest.path,
            branch_path(&repo.default_branch)
        ),
        None,
        false,
    )?;
    if branch.status == 404 {
        return Ok(false);
    }
    if expect(branch, &[200])?["name"] != repo.default_branch {
        return Err("GitHub branch identity mismatch".into());
    }
    let value = expect(
        call(
            io,
            dest,
            "PATCH",
            &format!("/repos/{}", dest.path),
            Some(json!({"default_branch":repo.default_branch})),
            false,
        )?,
        &[200],
    )?;
    if value["default_branch"] != repo.default_branch {
        return Err("GitHub default branch update not confirmed".into());
    }
    Ok(true)
}

/// The commit `branch` points at on the destination; `None` while the branch
/// (or the whole repository) does not exist yet.
pub fn head(io: &mut dyn Transport, dest: &Destination, branch: &str) -> Result<Option<String>> {
    let response = call(
        io,
        dest,
        "GET",
        &format!("/repos/{}/git/ref/heads/{}", dest.path, branch_path(branch)),
        None,
        false,
    )?;
    // 409: the repository is empty.
    if response.status == 404 || response.status == 409 {
        return Ok(None);
    }
    let value = expect(response, &[200])?;
    Ok(Some(
        value["object"]["sha"]
            .as_str()
            .ok_or("Missing GitHub ref object")?
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        fn methods(&self) -> Vec<&'static str> {
            self.asked.iter().map(|(method, _, _)| *method).collect()
        }
    }
    impl Transport for Script {
        fn send(&mut self, request: Request) -> Result<Response> {
            assert_eq!(request.endpoint.origin, "https://api.github.com");
            self.asked
                .push((request.method, request.path, request.body));
            let (status, body) = self.answers.remove(0);
            Ok(Response { status, body })
        }
    }

    fn dest(private: bool, id: Option<&str>) -> (Repository, Destination) {
        let repo: Repository = serde_json::from_value(json!({"path":"team/repo","source_id":1,"private":private,"default_branch":"main","content":"native-git","destinations":[]})).unwrap();
        let dest: Destination = serde_json::from_value(json!({"provider":"github","endpoint":{"origin":"https://api.github.com","token_env":"TOKEN"},"path":"team/repo","namespace":"team","repository_id":id,"mirror_user":"bot","password_env":"TOKEN","interval_seconds":3600})).unwrap();
        (repo, dest)
    }

    fn installed() -> Value {
        let mut lock = lock_body();
        lock["id"] = json!(5);
        lock["source_type"] = json!("Repository");
        lock["current_user_can_bypass"] = json!("always");
        lock
    }

    #[test]
    fn the_lock_reads_back_exactly_or_not_at_all() {
        assert!(is_lock(&installed()));
        let tweaks: [fn(&mut Value); 7] = [
            |v: &mut Value| v["enforcement"] = json!("evaluate"),
            |v: &mut Value| v["target"] = json!("tag"),
            |v: &mut Value| v["conditions"]["ref_name"]["include"] = json!(["~ALL"]),
            |v: &mut Value| v["conditions"]["ref_name"]["exclude"] = json!(["refs/heads/x"]),
            |v: &mut Value| v["bypass_actors"] = json!([]),
            |v: &mut Value| v["rules"] = json!([{"type":"update"}]),
            |v: &mut Value| v["rules"] = json!([{"type":"update"},{"type":"deletion"},{"type":"non_fast_forward"},{"type":"creation"}]),
        ];
        for tweak in tweaks {
            let mut lock = installed();
            tweak(&mut lock);
            assert!(!is_lock(&lock));
        }
        // GitHub reads the organisation-owner bypass back without an actor id.
        let mut lock = installed();
        lock["bypass_actors"][0]["actor_id"] = Value::Null;
        assert!(is_lock(&lock));
        // GitHub adds parameters to a rule it reads back: the type decides.
        let mut lock = installed();
        lock["rules"] = json!([{"type":"update","parameters":{"update_allows_fetch_and_merge":false}},{"type":"deletion"},{"type":"non_fast_forward"}]);
        assert!(is_lock(&lock));
    }

    fn principal() -> Vec<(u16, Value)> {
        vec![
            (200, json!({"login":"Bot"})),
            (200, json!({"role":"admin","state":"active"})),
        ]
    }

    #[test]
    fn a_missing_lock_is_created_and_only_the_default_branch_is_protected() {
        let (repo, dest) = dest(false, Some("7"));
        let mut answers = principal();
        answers.extend([
            (200, json!([])),
            (201, installed()),
            (200, installed()),
            (200, json!([{"name":"main"}])),
        ]);
        let mut io = Script::new(answers);
        assert!(protect(&mut io, &repo, &dest, true).unwrap());
        // The created ruleset is read back before it counts.
        assert_eq!(io.methods(), ["GET", "GET", "GET", "POST", "GET", "GET"]);
        assert_eq!(io.asked[4].1, "/repos/team/repo/rulesets/5");
        assert_eq!(io.asked[3].1, "/repos/team/repo/rulesets");
        assert_eq!(
            io.asked[3].2.as_ref().unwrap()["conditions"]["ref_name"]["include"],
            json!(["~DEFAULT_BRANCH"])
        );
    }

    #[test]
    fn planning_writes_nothing_and_a_lock_in_place_is_left_alone() {
        let (repo, dest) = dest(false, None);
        let mut answers = principal();
        answers.push((200, json!([])));
        let mut io = Script::new(answers);
        assert!(!protect(&mut io, &repo, &dest, false).unwrap());
        assert_eq!(io.methods(), ["GET", "GET", "GET"]);

        let mut answers = principal();
        answers.extend([
            (200, json!([{"id":5,"name":LOCK_NAME,"source_type":"Repository","target":"branch","enforcement":"active"}])),
            (200, installed()),
            (200, json!([{"name":"main"}])),
        ]);
        let mut io = Script::new(answers);
        assert!(protect(&mut io, &repo, &dest, true).unwrap());
        assert!(io.methods().iter().all(|m| *m == "GET"));
    }

    #[test]
    fn a_drifted_lock_is_replaced_in_place() {
        let (repo, dest) = dest(false, None);
        let mut drifted = installed();
        drifted["rules"] = json!([{"type":"update"}]);
        let mut answers = principal();
        answers.extend([
            (200, json!([{"id":5,"name":LOCK_NAME,"source_type":"Repository","target":"branch","enforcement":"active"}])),
            (200, drifted),
            (200, installed()),
            (200, installed()),
            (200, json!([{"name":"main"}])),
        ]);
        let mut io = Script::new(answers);
        assert!(protect(&mut io, &repo, &dest, true).unwrap());
        assert_eq!(io.asked[4].0, "PUT");
        assert_eq!(io.asked[4].1, "/repos/team/repo/rulesets/5");
    }

    #[test]
    fn foreign_protection_and_a_principal_that_cannot_bypass_stop_the_enrollment() {
        let (repo, dest) = dest(false, None);
        // Another active branch ruleset (here the organisation's).
        let mut answers = principal();
        answers.push((200, json!([{"id":9,"name":"other","source_type":"Organization","target":"branch","enforcement":"active"}])));
        assert!(protect(&mut Script::new(answers), &repo, &dest, true).is_err());
        // A classic protection on another branch.
        let mut answers = principal();
        answers.extend([
            (200, json!([{"id":5,"name":LOCK_NAME,"source_type":"Repository","target":"branch","enforcement":"active"}])),
            (200, installed()),
            (200, json!([{"name":"main"},{"name":"release"}])),
        ]);
        assert!(protect(&mut Script::new(answers), &repo, &dest, true).is_err());
        // The credential is somebody else, or not an owner.
        let answers = vec![(200, json!({"login":"someone"}))];
        assert!(protect(&mut Script::new(answers), &repo, &dest, true).is_err());
        let answers = vec![
            (200, json!({"login":"bot"})),
            (200, json!({"role":"member","state":"active"})),
        ];
        assert!(protect(&mut Script::new(answers), &repo, &dest, true).is_err());
    }

    #[test]
    fn a_credential_that_cannot_bypass_the_lock_stops_the_enrollment() {
        let (repo, dest) = dest(false, None);
        for bypass in ["never", "pull_requests_only"] {
            let mut weak = installed();
            weak["current_user_can_bypass"] = json!(bypass);
            let mut answers = principal();
            answers.extend([
                (200, json!([{"id":5,"name":LOCK_NAME,"source_type":"Repository","target":"branch","enforcement":"active"}])),
                (200, weak),
            ]);
            assert!(protect(&mut Script::new(answers), &repo, &dest, true).is_err());
        }
    }

    #[test]
    fn a_private_repository_on_a_free_organisation_cannot_be_locked_and_is_not_probed() {
        let (repo, dest) = dest(true, None);
        let mut io = Script::new(vec![(200, json!({"plan":{"name":"free"}}))]);
        assert!(!protect(&mut io, &repo, &dest, true).unwrap());
        assert_eq!(io.methods(), ["GET"]);
    }

    #[test]
    fn an_existing_repository_never_uses_the_creation_scope_and_actions_go_off() {
        let (repo, dest) = dest(false, Some("7"));
        let mut io = Script::new(vec![
            (
                200,
                json!({"id":7,"full_name":"Team/Repo","private":false,"archived":false}),
            ),
            (200, json!({"enabled":true})),
            (204, Value::Null),
        ]);
        assert!(ensure_destination_repo(&mut io, &repo, &dest, true, false)
            .unwrap()
            .is_some());
        assert_eq!(io.methods(), ["GET", "GET", "PUT"]);
        assert_eq!(io.asked[2].2, Some(json!({"enabled":false})));
        // A repository that disappeared is never replaced; an undeclared one is created.
        let mut io = Script::new(vec![(404, Value::Null)]);
        assert!(ensure_destination_repo(&mut io, &repo, &dest, true, true).is_err());
        let (repo, dest) = dest_without_id();
        let mut io = Script::new(vec![(404, Value::Null)]);
        assert!(ensure_destination_repo(&mut io, &repo, &dest, true, false)
            .unwrap()
            .is_none());
        assert_eq!(io.methods(), ["GET"]);
    }

    fn dest_without_id() -> (Repository, Destination) {
        dest(false, None)
    }

    #[test]
    fn visibility_and_identity_mismatches_are_refused() {
        let (repo, dest) = dest(false, Some("7"));
        for body in [
            json!({"id":7,"full_name":"team/repo","private":true}),
            json!({"id":8,"full_name":"team/repo","private":false}),
            json!({"id":7,"full_name":"team/other","private":false}),
            json!({"id":7,"full_name":"team/repo","private":false,"archived":true}),
        ] {
            assert!(verify(body, &repo, &dest).is_err());
        }
    }

    #[test]
    fn a_renamed_repository_is_reported_and_never_renamed() {
        let (_, dest) = dest(false, Some("7"));
        let mut io = Script::new(vec![(200, json!({"id":7,"full_name":"team/old"}))]);
        assert_eq!(
            naming(&mut io, &dest, true).unwrap(),
            Naming::Blocked {
                from: "team/old".into(),
                reason:
                    "GitHub repositories are not renamed by cfrg; rename it in GitHub or pin the path"
            }
        );
        assert_eq!(io.methods(), ["GET"]);
        let mut io = Script::new(vec![(200, json!({"id":7,"full_name":"Team/Repo"}))]);
        assert_eq!(naming(&mut io, &dest, true).unwrap(), Naming::Current);
    }

    #[test]
    fn heads_read_the_ref_and_an_empty_repository_has_none() {
        let (_, dest) = dest(false, None);
        let mut io = Script::new(vec![(200, json!({"object":{"sha":"abc"}}))]);
        assert_eq!(
            head(&mut io, &dest, "ci/x").unwrap().as_deref(),
            Some("abc")
        );
        assert_eq!(io.asked[0].1, "/repos/team/repo/git/ref/heads/ci/x");
        let mut io = Script::new(vec![(409, Value::Null)]);
        assert_eq!(head(&mut io, &dest, "main").unwrap(), None);
    }
}
