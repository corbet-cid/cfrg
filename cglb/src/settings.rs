//! R42 on a GitLab copy: read what the project shows, write what the plan decided. A project
//! is addressed by its URL-encoded full path. One `PUT /projects/:id` carries every field
//! GitLab takes together; archiving is its own call.
use cfrg::{
    native::{
        encode,
        http::{expect, Auth, Request, Response, Transport},
        settings::{Applied, Change, Field, Observed, Primary, Protection, Target},
        Provider,
    },
    Result,
};
use serde_json::{json, Map, Value};

fn call(
    io: &mut dyn Transport,
    target: &Target,
    method: &'static str,
    suffix: &str,
    body: Option<Value>,
) -> Result<Response> {
    if target.provider != Provider::Gitlab {
        return Err("GitLab adapter requires a GitLab target".into());
    }
    io.send(Request {
        endpoint: target.endpoint.clone(),
        auth: Auth::PrivateToken,
        method,
        path: format!("/api/v4/projects/{}{suffix}", encode(&target.path)),
        body,
        scope: format!("{}/api", target.endpoint.origin),
        creation: false,
    })
}

fn level_on(value: &Value) -> Option<bool> {
    value.as_str().map(|level| level != "disabled")
}

/// Read the project. `Ok(None)`: it does not exist (404).
pub fn observe(io: &mut dyn Transport, target: &Target) -> Result<Option<Observed>> {
    let response = call(io, target, "GET", "", None)?;
    if response.status == 404 {
        return Ok(None);
    }
    let project = expect(response, &[200])?;
    if !project["path_with_namespace"]
        .as_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(&target.path))
    {
        return Err("GitLab copy identity mismatch".into());
    }
    let topics = project["topics"].as_array().map(|rows| {
        rows.iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect()
    });
    Ok(Some(Observed {
        // `internal` is open to every signed-in user: it is not private.
        private: project["visibility"].as_str().map(|v| v == "private"),
        description: Some(
            project["description"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        ),
        homepage: None,
        topics,
        // An empty project has no default branch to compare.
        default_branch: if project["empty_repo"] == true {
            None
        } else {
            project["default_branch"].as_str().map(String::from)
        },
        archived: project["archived"].as_bool(),
        issues: level_on(&project["issues_access_level"]),
        wiki: level_on(&project["wiki_access_level"]),
        ci: level_on(&project["builds_access_level"]),
        updates: None,
        vulnerability_reporting: None,
    }))
}

/// The commit a branch of the project points at, or `None` when the branch is not there yet.
pub fn head(io: &mut dyn Transport, target: &Target, branch: &str) -> Result<Option<String>> {
    let response = call(
        io,
        target,
        "GET",
        &format!("/repository/branches/{}", encode(branch)),
        None,
    )?;
    if response.status == 404 {
        return Ok(None);
    }
    Ok(expect(response, &[200])?["commit"]["id"]
        .as_str()
        .map(String::from))
}

/// Whether the default branch is held for the mirroring alone: a protected branch whose push
/// and merge levels are "No one" (0), or a single deploy key.
pub fn protection(io: &mut dyn Transport, target: &Target, branch: &str) -> Result<Protection> {
    let response = call(
        io,
        target,
        "GET",
        &format!("/protected_branches/{}", encode(branch)),
        None,
    )?;
    if response.status == 404 {
        return Ok(Protection::Missing);
    }
    let rule = expect(response, &[200])?;
    let levels = |key: &str| -> Vec<Value> { rule[key].as_array().cloned().unwrap_or_default() };
    let closed = |rows: &[Value], keys_ok: bool| {
        rows.iter()
            .all(|r| r["access_level"] == 0 || (keys_ok && r["deploy_key_id"].as_u64().is_some()))
    };
    Ok(
        if closed(&levels("push_access_levels"), true)
            && closed(&levels("merge_access_levels"), false)
        {
            Protection::Locked
        } else {
            Protection::Restricted {
                detail: "protected, but a role can push or merge".into(),
            }
        },
    )
}

fn put(io: &mut dyn Transport, target: &Target, body: Map<String, Value>) -> Result<()> {
    expect(
        call(io, target, "PUT", "", Some(Value::Object(body)))?,
        &[200],
    )?;
    Ok(())
}

/// Write `changes` (already in application order) to the project.
pub fn apply(
    io: &mut dyn Transport,
    target: &Target,
    primary: &Primary,
    changes: &[Change],
) -> Result<Vec<Applied>> {
    let mut done = Vec::new();
    let mut fields = Map::new();
    let mut together = Vec::new();
    for change in changes {
        match change.field {
            Field::Unarchive => {
                expect(call(io, target, "POST", "/unarchive", None)?, &[201])?;
                done.push(Applied::done(Field::Unarchive));
            }
            Field::Visibility => {
                fields.insert("visibility".into(), change.to.clone());
                together.push(Field::Visibility);
            }
            Field::Description => {
                fields.insert("description".into(), change.to.clone());
                together.push(Field::Description);
            }
            Field::Topics => {
                fields.insert("topics".into(), change.to.clone());
                together.push(Field::Topics);
            }
            Field::Issues => {
                fields.insert("issues_access_level".into(), json!("enabled"));
                together.push(Field::Issues);
            }
            Field::Wiki => {
                let level = if change.to == true {
                    "enabled"
                } else {
                    "disabled"
                };
                fields.insert("wiki_access_level".into(), json!(level));
                together.push(Field::Wiki);
            }
            Field::Ci => {
                fields.insert("builds_access_level".into(), json!("disabled"));
                together.push(Field::Ci);
            }
            Field::DefaultBranch => {
                if head(io, target, &primary.default_branch)?.is_some() {
                    fields.insert("default_branch".into(), json!(primary.default_branch));
                    together.push(Field::DefaultBranch);
                } else {
                    done.push(Applied::skipped(
                        Field::DefaultBranch,
                        "the branch is not on the copy yet",
                    ));
                }
            }
            Field::Homepage | Field::Updates | Field::VulnerabilityReporting => {
                done.push(Applied::skipped(change.field, "GitLab cannot hold it"));
            }
            Field::Archive => {
                if !fields.is_empty() {
                    put(io, target, std::mem::take(&mut fields))?;
                    done.extend(together.drain(..).map(Applied::done));
                }
                expect(call(io, target, "POST", "/archive", None)?, &[201])?;
                done.push(Applied::done(Field::Archive));
            }
        }
    }
    if !fields.is_empty() {
        put(io, target, fields)?;
        done.extend(together.drain(..).map(Applied::done));
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfrg::native::Endpoint;

    struct Script {
        answers: Vec<(u16, Value)>,
        asked: Vec<(&'static str, String, Option<Value>)>,
    }
    impl Transport for Script {
        fn send(&mut self, request: Request) -> Result<Response> {
            self.asked
                .push((request.method, request.path, request.body));
            let (status, body) = self.answers.remove(0);
            Ok(Response { status, body })
        }
    }
    fn target() -> Target {
        Target {
            provider: Provider::Gitlab,
            endpoint: Endpoint {
                origin: "https://gitlab.com".into(),
                token_env: "TOKEN".into(),
            },
            path: "group/repo".into(),
        }
    }
    fn primary() -> Primary {
        Primary {
            private: false,
            description: String::new(),
            homepage: String::new(),
            topics: vec![],
            default_branch: "main".into(),
            archived: false,
        }
    }

    #[test]
    fn observe_maps_access_levels_and_ignores_the_branch_of_an_empty_project() {
        let mut io = Script {
            answers: vec![(
                200,
                json!({"path_with_namespace":"Group/Repo","visibility":"internal","description":null,
                "topics":["a"],"default_branch":"main","empty_repo":true,"archived":false,
                "issues_access_level":"enabled","wiki_access_level":"disabled","builds_access_level":"private"}),
            )],
            asked: vec![],
        };
        let seen = observe(&mut io, &target()).unwrap().unwrap();
        assert_eq!(seen.private, Some(false));
        assert_eq!(seen.default_branch, None);
        assert_eq!(seen.issues, Some(true));
        assert_eq!(seen.wiki, Some(false));
        assert_eq!(seen.ci, Some(true));
        assert_eq!(io.asked[0].1, "/api/v4/projects/group%2Frepo");
    }

    #[test]
    fn one_put_carries_the_fields_and_archiving_follows_it() {
        let change = |field, to: Value| Change {
            field,
            from: Value::Null,
            to,
        };
        let changes = [
            change(Field::Visibility, json!("private")),
            change(Field::Description, json!("d")),
            change(Field::Topics, json!(["x"])),
            change(Field::Wiki, json!(true)),
            change(Field::Ci, json!("off")),
            change(Field::Archive, json!(true)),
        ];
        let mut io = Script {
            answers: vec![(200, json!({})), (201, json!({}))],
            asked: vec![],
        };
        let done = apply(&mut io, &target(), &primary(), &changes).unwrap();
        assert_eq!(done.len(), 6);
        assert_eq!(io.asked[0].0, "PUT");
        assert_eq!(
            io.asked[0].2,
            Some(
                json!({"visibility":"private","description":"d","topics":["x"],
                "wiki_access_level":"enabled","builds_access_level":"disabled"})
            )
        );
        assert_eq!(io.asked[1].1, "/api/v4/projects/group%2Frepo/archive");
    }

    #[test]
    fn unarchive_is_the_first_call() {
        let changes = [
            Change {
                field: Field::Unarchive,
                from: json!(true),
                to: json!(false),
            },
            Change {
                field: Field::Description,
                from: json!("a"),
                to: json!("b"),
            },
        ];
        let mut io = Script {
            answers: vec![(201, json!({})), (200, json!({}))],
            asked: vec![],
        };
        apply(&mut io, &target(), &primary(), &changes).unwrap();
        assert_eq!(io.asked[0].1, "/api/v4/projects/group%2Frepo/unarchive");
        assert_eq!(io.asked[1].0, "PUT");
    }

    #[test]
    fn protection_is_locked_only_when_nobody_but_a_deploy_key_can_push() {
        let mut io = Script {
            answers: vec![(404, Value::Null)],
            asked: vec![],
        };
        assert_eq!(
            protection(&mut io, &target(), "main").unwrap(),
            Protection::Missing
        );
        let locked = json!({"push_access_levels":[{"access_level":40,"deploy_key_id":7}],
            "merge_access_levels":[{"access_level":0}]});
        let mut io = Script {
            answers: vec![(200, locked)],
            asked: vec![],
        };
        assert_eq!(
            protection(&mut io, &target(), "main").unwrap(),
            Protection::Locked
        );
        let open = json!({"push_access_levels":[{"access_level":40}],
            "merge_access_levels":[{"access_level":40}]});
        let mut io = Script {
            answers: vec![(200, open)],
            asked: vec![],
        };
        assert!(matches!(
            protection(&mut io, &target(), "main").unwrap(),
            Protection::Restricted { .. }
        ));
    }
}
