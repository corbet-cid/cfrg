//! R42 on a GitHub copy: read what the copy shows, write what the plan decided. GitHub is a
//! copy only, never a primary. Every write is one paced mutation; related repository fields
//! travel in one PATCH so a copy costs as few requests as possible.
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
    if target.provider != Provider::Github || target.endpoint.origin != "https://api.github.com" {
        return Err("GitHub API origin required".into());
    }
    io.send(Request {
        endpoint: target.endpoint.clone(),
        auth: Auth::Bearer,
        method,
        path: format!("/repos/{}{suffix}", target.path),
        body,
        scope: format!("{}/api", target.endpoint.origin),
        creation: false,
    })
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// Read the copy. `Ok(None)`: it does not exist (404). A moved repository answers 301 and
/// is an error: the copy is then not where the placement says.
pub fn observe(io: &mut dyn Transport, target: &Target) -> Result<Option<Observed>> {
    let response = call(io, target, "GET", "", None)?;
    if response.status == 404 {
        return Ok(None);
    }
    let repo = expect(response, &[200])?;
    if !repo["full_name"]
        .as_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(&target.path))
    {
        return Err("GitHub copy identity mismatch".into());
    }
    let topics = repo["topics"].as_array().map(|rows| {
        rows.iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect()
    });
    // The Actions switch is its own resource.
    let actions = expect(
        call(io, target, "GET", "/actions/permissions", None)?,
        &[200],
    )?;
    // Private vulnerability reporting is only offered on public repositories that can still
    // be changed; an archived one (or one the API refuses with 404 or 422) is left alone.
    let reporting = if repo["private"] == false && repo["archived"] != true {
        let response = call(io, target, "GET", "/private-vulnerability-reporting", None)?;
        match response.status {
            404 | 422 => None,
            _ => expect(response, &[200])?["enabled"].as_bool(),
        }
    } else {
        None
    };
    Ok(Some(Observed {
        private: repo["private"].as_bool(),
        description: Some(text(&repo["description"])),
        homepage: Some(text(&repo["homepage"])),
        topics,
        default_branch: repo["default_branch"].as_str().map(String::from),
        archived: repo["archived"].as_bool(),
        issues: repo["has_issues"].as_bool(),
        wiki: repo["has_wiki"].as_bool(),
        ci: actions["enabled"].as_bool(),
        // Dependabot security updates; absent when the credential cannot see the setting.
        updates: repo["security_and_analysis"]["dependabot_security_updates"]["status"]
            .as_str()
            .map(|status| status == "enabled"),
        vulnerability_reporting: reporting,
    }))
}

/// The commit a branch of the copy points at, or `None` when the branch is not there yet.
pub fn head(io: &mut dyn Transport, target: &Target, branch: &str) -> Result<Option<String>> {
    let response = call(
        io,
        target,
        "GET",
        &format!("/branches/{}", encode(branch)),
        None,
    )?;
    if response.status == 404 {
        return Ok(None);
    }
    Ok(expect(response, &[200])?["commit"]["sha"]
        .as_str()
        .map(String::from))
}

/// Whether the default branch is held for the mirroring alone. A private repository in a free
/// organisation has no rulesets and the API answers 403, which stops the whole run: it is
/// not asked.
pub fn protection(
    io: &mut dyn Transport,
    target: &Target,
    private: bool,
    branch: &str,
) -> Result<Protection> {
    if private {
        return Ok(Protection::Unavailable {
            reason: "rulesets need a paid plan on a private repository; not asked".into(),
        });
    }
    let rulesets = expect(call(io, target, "GET", "/rulesets", None)?, &[200])?;
    let rows = rulesets.as_array().cloned().unwrap_or_default();
    if let Some(row) = rows
        .iter()
        .find(|r| r["name"] == crate::native::LOCK_NAME && r["enforcement"] == "active")
    {
        let id = row["id"].as_u64().ok_or("Missing ruleset id")?;
        let detail = expect(
            call(io, target, "GET", &format!("/rulesets/{id}"), None)?,
            &[200],
        )?;
        return Ok(if crate::native::is_lock(&detail) {
            Protection::Locked
        } else {
            Protection::Restricted {
                detail: "a ruleset named like the lock, not exactly the lock".into(),
            }
        });
    }
    if rows.iter().any(|r| r["enforcement"] == "active") {
        return Ok(Protection::Restricted {
            detail: "other active rulesets".into(),
        });
    }
    let classic = call(
        io,
        target,
        "GET",
        &format!("/branches/{}/protection", encode(branch)),
        None,
    )?;
    Ok(match classic.status {
        200 => Protection::Restricted {
            detail: "classic branch protection".into(),
        },
        404 => Protection::Missing,
        _ => {
            expect(classic, &[200])?;
            Protection::Missing
        }
    })
}

fn patch(io: &mut dyn Transport, target: &Target, body: Map<String, Value>) -> Result<()> {
    expect(
        call(io, target, "PATCH", "", Some(Value::Object(body)))?,
        &[200],
    )?;
    Ok(())
}

/// Write `changes` (already in application order) to the copy.
pub fn apply(
    io: &mut dyn Transport,
    target: &Target,
    primary: &Primary,
    changes: &[Change],
) -> Result<Vec<Applied>> {
    let mut done = Vec::new();
    // Fields that GitHub takes in one repository PATCH.
    let mut fields = Map::new();
    let mut together = Vec::new();
    let flush = |io: &mut dyn Transport,
                 fields: &mut Map<String, Value>,
                 together: &mut Vec<Field>,
                 done: &mut Vec<Applied>|
     -> Result<()> {
        if !fields.is_empty() {
            patch(io, target, std::mem::take(fields))?;
            done.extend(together.drain(..).map(Applied::done));
        }
        Ok(())
    };
    for change in changes {
        match change.field {
            Field::Unarchive => {
                patch(
                    io,
                    target,
                    Map::from_iter([("archived".into(), json!(false))]),
                )?;
                done.push(Applied::done(Field::Unarchive));
            }
            Field::Visibility => {
                flush(io, &mut fields, &mut together, &mut done)?;
                patch(
                    io,
                    target,
                    Map::from_iter([("private".into(), json!(change.to == "private"))]),
                )?;
                done.push(Applied::done(Field::Visibility));
            }
            Field::Description => {
                fields.insert("description".into(), change.to.clone());
                together.push(Field::Description);
            }
            Field::Homepage => {
                fields.insert("homepage".into(), change.to.clone());
                together.push(Field::Homepage);
            }
            Field::Issues => {
                fields.insert("has_issues".into(), json!(true));
                together.push(Field::Issues);
            }
            Field::Wiki => {
                fields.insert("has_wiki".into(), json!(change.to == true));
                together.push(Field::Wiki);
            }
            Field::DefaultBranch => {
                // The branch must be on the copy; the mirror delivers it.
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
            Field::Topics => {
                flush(io, &mut fields, &mut together, &mut done)?;
                expect(
                    call(
                        io,
                        target,
                        "PUT",
                        "/topics",
                        Some(json!({"names":change.to})),
                    )?,
                    &[200],
                )?;
                done.push(Applied::done(Field::Topics));
            }
            Field::Ci => {
                flush(io, &mut fields, &mut together, &mut done)?;
                expect(
                    call(
                        io,
                        target,
                        "PUT",
                        "/actions/permissions",
                        Some(json!({"enabled":false})),
                    )?,
                    &[204],
                )?;
                done.push(Applied::done(Field::Ci));
            }
            Field::VulnerabilityReporting => {
                flush(io, &mut fields, &mut together, &mut done)?;
                expect(
                    call(io, target, "PUT", "/private-vulnerability-reporting", None)?,
                    &[204],
                )?;
                done.push(Applied::done(Field::VulnerabilityReporting));
            }
            Field::Updates => {
                flush(io, &mut fields, &mut together, &mut done)?;
                expect(
                    call(io, target, "DELETE", "/automated-security-fixes", None)?,
                    &[204],
                )?;
                done.push(Applied::done(Field::Updates));
            }
            Field::Archive => {
                flush(io, &mut fields, &mut together, &mut done)?;
                patch(
                    io,
                    target,
                    Map::from_iter([("archived".into(), json!(true))]),
                )?;
                done.push(Applied::done(Field::Archive));
            }
        }
    }
    flush(io, &mut fields, &mut together, &mut done)?;
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
            provider: Provider::Github,
            endpoint: Endpoint {
                origin: "https://api.github.com".into(),
                token_env: "TOKEN".into(),
            },
            path: "org/repo".into(),
        }
    }
    fn primary() -> Primary {
        Primary {
            private: false,
            description: "d".into(),
            homepage: String::new(),
            topics: vec![],
            default_branch: "main".into(),
            archived: false,
        }
    }

    #[test]
    fn observe_reads_the_repository_and_the_actions_switch() {
        let mut io = Script {
            answers: vec![
                (
                    200,
                    json!({"full_name":"Org/Repo","private":true,"description":null,"homepage":"",
                    "topics":["rust"],"default_branch":"main","archived":false,"has_issues":false,"has_wiki":true,
                    "security_and_analysis":{"dependabot_security_updates":{"status":"enabled"}}}),
                ),
                (200, json!({"enabled":true})),
            ],
            asked: vec![],
        };
        let seen = observe(&mut io, &target()).unwrap().unwrap();
        assert_eq!(seen.private, Some(true));
        assert_eq!(seen.description.as_deref(), Some(""));
        assert_eq!(seen.issues, Some(false));
        assert_eq!(seen.ci, Some(true));
        assert_eq!(seen.updates, Some(true));
        assert_eq!(seen.topics, Some(vec!["rust".to_string()]));
        assert_eq!(io.asked[1].1, "/repos/org/repo/actions/permissions");
        // A missing copy is not an error.
        let mut gone = Script {
            answers: vec![(404, Value::Null)],
            asked: vec![],
        };
        assert!(observe(&mut gone, &target()).unwrap().is_none());
    }

    #[test]
    fn a_public_copy_also_reports_private_vulnerability_reporting() {
        let mut io = Script {
            answers: vec![
                (
                    200,
                    json!({"full_name":"org/repo","private":false,"description":"","homepage":"","topics":[],
                    "default_branch":"main","archived":false,"has_issues":true,"has_wiki":true}),
                ),
                (200, json!({"enabled":false})),
                (200, json!({"enabled":false})),
            ],
            asked: vec![],
        };
        let seen = observe(&mut io, &target()).unwrap().unwrap();
        assert_eq!(seen.vulnerability_reporting, Some(false));
        assert_eq!(seen.wiki, Some(true));
        assert_eq!(
            io.asked[2].1,
            "/repos/org/repo/private-vulnerability-reporting"
        );
        // Switching it on is one PUT without a body.
        let changes = [Change {
            field: Field::VulnerabilityReporting,
            from: json!(false),
            to: json!(true),
        }];
        let mut io = Script {
            answers: vec![(204, Value::Null)],
            asked: vec![],
        };
        apply(&mut io, &target(), &primary(), &changes).unwrap();
        assert_eq!(io.asked[0].0, "PUT");
        assert_eq!(
            io.asked[0].1,
            "/repos/org/repo/private-vulnerability-reporting"
        );
    }

    #[test]
    fn an_archived_or_refused_repository_has_no_reporting_state_and_is_no_error() {
        let public = |archived: bool| {
            json!({"full_name":"org/repo","private":false,"description":"","homepage":"","topics":[],
            "default_branch":"main","archived":archived,"has_issues":true,"has_wiki":false})
        };
        // Archived: the reporting switch is not even asked.
        let mut io = Script {
            answers: vec![(200, public(true)), (200, json!({"enabled":true}))],
            asked: vec![],
        };
        let seen = observe(&mut io, &target()).unwrap().unwrap();
        assert_eq!(seen.vulnerability_reporting, None);
        assert_eq!(io.asked.len(), 2);
        // The API answers 422 for one repository: unknown, not a failure of the run.
        let mut io = Script {
            answers: vec![
                (200, public(false)),
                (200, json!({"enabled":true})),
                (422, Value::Null),
            ],
            asked: vec![],
        };
        let seen = observe(&mut io, &target()).unwrap().unwrap();
        assert_eq!(seen.vulnerability_reporting, None);
    }

    #[test]
    fn the_wiki_is_written_with_the_planned_value() {
        let changes = [Change {
            field: Field::Wiki,
            from: json!(true),
            to: json!(false),
        }];
        let mut io = Script {
            answers: vec![(200, json!({}))],
            asked: vec![],
        };
        apply(&mut io, &target(), &primary(), &changes).unwrap();
        assert_eq!(io.asked[0].2, Some(json!({"has_wiki":false})));
    }

    #[test]
    fn related_fields_share_one_patch_and_the_rest_are_separate_calls() {
        let change = |field, to: Value| Change {
            field,
            from: Value::Null,
            to,
        };
        let changes = [
            change(Field::Visibility, json!("private")),
            change(Field::Description, json!("new")),
            change(Field::Issues, json!(true)),
            change(Field::Topics, json!(["a"])),
            change(Field::Ci, json!("off")),
            change(Field::Archive, json!(true)),
        ];
        let mut io = Script {
            answers: vec![
                (200, json!({})), // visibility
                (200, json!({})), // description + issues
                (200, json!({})), // topics
                (204, Value::Null),
                (200, json!({})), // archive
            ],
            asked: vec![],
        };
        let done = apply(&mut io, &target(), &primary(), &changes).unwrap();
        assert_eq!(done.len(), 6);
        let calls: Vec<_> = io.asked.iter().map(|(m, p, _)| (*m, p.as_str())).collect();
        assert_eq!(
            calls,
            [
                ("PATCH", "/repos/org/repo"),
                ("PATCH", "/repos/org/repo"),
                ("PUT", "/repos/org/repo/topics"),
                ("PUT", "/repos/org/repo/actions/permissions"),
                ("PATCH", "/repos/org/repo"),
            ]
        );
        assert_eq!(io.asked[0].2, Some(json!({"private":true})));
        assert_eq!(
            io.asked[1].2,
            Some(json!({"description":"new","has_issues":true}))
        );
        assert_eq!(io.asked[4].2, Some(json!({"archived":true})));
    }

    #[test]
    fn a_default_branch_that_is_not_on_the_copy_yet_is_skipped_not_forced() {
        let changes = [Change {
            field: Field::DefaultBranch,
            from: json!("master"),
            to: json!("main"),
        }];
        let mut io = Script {
            answers: vec![(404, Value::Null)],
            asked: vec![],
        };
        let done = apply(&mut io, &target(), &primary(), &changes).unwrap();
        assert_eq!(done[0].outcome, "skipped");
        assert_eq!(io.asked.len(), 1, "no PATCH for a branch the copy lacks");
    }

    #[test]
    fn protection_asks_nothing_of_a_private_repository_and_reads_a_public_one() {
        let mut io = Script {
            answers: vec![],
            asked: vec![],
        };
        let private = protection(&mut io, &target(), true, "main").unwrap();
        assert!(matches!(private, Protection::Unavailable { .. }));
        assert!(io.asked.is_empty(), "a 403 would stop the run");
        let mut io = Script {
            answers: vec![(200, json!([])), (404, Value::Null)],
            asked: vec![],
        };
        assert_eq!(
            protection(&mut io, &target(), false, "main").unwrap(),
            Protection::Missing
        );
        assert_eq!(io.asked[1].1, "/repos/org/repo/branches/main/protection");
        let mut io = Script {
            answers: vec![(200, json!([{"id":1,"name":"other","enforcement":"active"}]))],
            asked: vec![],
        };
        assert!(matches!(
            protection(&mut io, &target(), false, "main").unwrap(),
            Protection::Restricted { .. }
        ));
    }
}
