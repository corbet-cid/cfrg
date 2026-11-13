//! R42 on a Bitbucket copy: read what the repository shows, write what the plan decided.
//! Bitbucket has no topics, no archived state and no update tooling; the homepage is its
//! `website`. A repository without a commit has no pipelines configuration yet (404): there
//! is nothing to switch off.
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
    if target.provider != Provider::Bitbucket
        || target.endpoint.origin != "https://api.bitbucket.org"
    {
        return Err("Bitbucket Cloud API origin required".into());
    }
    let workspace = target.path.split('/').next().ok_or("Missing workspace")?;
    io.send(Request {
        endpoint: target.endpoint.clone(),
        auth: Auth::Bearer,
        method,
        path: format!("/2.0/repositories/{}{suffix}", target.path),
        body,
        scope: format!("{}/workspace/{workspace}", target.endpoint.origin),
        creation: false,
    })
}

/// Read the repository. `Ok(None)`: it does not exist (404), or its workspace does not.
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
        return Err("Bitbucket copy identity mismatch".into());
    }
    let pipelines = call(io, target, "GET", "/pipelines_config", None)?;
    let ci = if pipelines.status == 404 {
        Some(false)
    } else {
        expect(pipelines, &[200])?["enabled"].as_bool()
    };
    Ok(Some(Observed {
        private: repo["is_private"].as_bool(),
        description: Some(repo["description"].as_str().unwrap_or_default().to_string()),
        homepage: Some(repo["website"].as_str().unwrap_or_default().to_string()),
        topics: None,
        default_branch: repo["mainbranch"]["name"].as_str().map(String::from),
        archived: None,
        issues: repo["has_issues"].as_bool(),
        wiki: repo["has_wiki"].as_bool(),
        ci,
        updates: None,
    }))
}

/// The commit a branch of the copy points at, or `None` when the branch is not there yet.
pub fn head(io: &mut dyn Transport, target: &Target, branch: &str) -> Result<Option<String>> {
    let response = call(
        io,
        target,
        "GET",
        &format!("/refs/branches/{}", encode(branch)),
        None,
    )?;
    if response.status == 404 {
        return Ok(None);
    }
    Ok(expect(response, &[200])?["target"]["hash"]
        .as_str()
        .map(String::from))
}

/// Whether the branches are held for the mirroring alone: a push restriction (an empty user
/// list freezes, one user is the mirror) and a merge restriction.
pub fn protection(io: &mut dyn Transport, target: &Target) -> Result<Protection> {
    let rules = expect(
        call(io, target, "GET", "/branch-restrictions?pagelen=100", None)?,
        &[200],
    )?;
    let rows = rules["values"].as_array().cloned().unwrap_or_default();
    let has = |kind: &str| rows.iter().any(|r| r["kind"] == kind);
    Ok(match (has("push"), has("restrict_merges")) {
        (true, true) => Protection::Locked,
        (false, false) => Protection::Missing,
        (push, _) => Protection::Restricted {
            detail: format!("only a {} restriction", if push { "push" } else { "merge" }),
        },
    })
}

/// Write `changes` (already in application order) to the repository.
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
            Field::Visibility => {
                // A private repository may not allow public forks.
                fields.insert("is_private".into(), json!(change.to == "private"));
                if change.to == "private" {
                    fields.insert("fork_policy".into(), json!("no_public_forks"));
                }
                together.push(Field::Visibility);
            }
            Field::Description => {
                fields.insert("description".into(), change.to.clone());
                together.push(Field::Description);
            }
            Field::Homepage => {
                fields.insert("website".into(), change.to.clone());
                together.push(Field::Homepage);
            }
            Field::Issues => {
                fields.insert("has_issues".into(), json!(true));
                together.push(Field::Issues);
            }
            Field::Wiki => {
                fields.insert("has_wiki".into(), json!(true));
                together.push(Field::Wiki);
            }
            Field::DefaultBranch => {
                if head(io, target, &primary.default_branch)?.is_some() {
                    fields.insert("mainbranch".into(), json!({"name":primary.default_branch}));
                    together.push(Field::DefaultBranch);
                } else {
                    done.push(Applied::skipped(
                        Field::DefaultBranch,
                        "the branch is not on the copy yet",
                    ));
                }
            }
            Field::Ci => {
                let saved = expect(
                    call(
                        io,
                        target,
                        "PUT",
                        "/pipelines_config",
                        Some(json!({"enabled":false})),
                    )?,
                    &[200],
                )?;
                if saved["enabled"] != false {
                    return Err("Bitbucket pipelines remain enabled".into());
                }
                done.push(Applied::done(Field::Ci));
            }
            Field::Topics | Field::Updates | Field::Archive | Field::Unarchive => {
                done.push(Applied::skipped(change.field, "Bitbucket cannot hold it"));
            }
        }
    }
    if !fields.is_empty() {
        expect(
            call(io, target, "PUT", "", Some(Value::Object(fields)))?,
            &[200],
        )?;
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
            provider: Provider::Bitbucket,
            endpoint: Endpoint {
                origin: "https://api.bitbucket.org".into(),
                token_env: "TOKEN".into(),
            },
            path: "ws/repo".into(),
        }
    }
    fn primary() -> Primary {
        Primary {
            private: true,
            description: String::new(),
            homepage: String::new(),
            topics: vec![],
            default_branch: "main".into(),
            archived: false,
        }
    }

    #[test]
    fn observe_reads_the_repository_and_treats_missing_pipelines_as_off() {
        let mut io = Script {
            answers: vec![
                (
                    200,
                    json!({"full_name":"ws/repo","is_private":true,"description":"d","website":null,
                    "mainbranch":{"name":"main"},"has_issues":true,"has_wiki":false}),
                ),
                (404, Value::Null),
            ],
            asked: vec![],
        };
        let seen = observe(&mut io, &target()).unwrap().unwrap();
        assert_eq!(seen.homepage.as_deref(), Some(""));
        assert_eq!(seen.default_branch.as_deref(), Some("main"));
        assert_eq!(seen.wiki, Some(false));
        assert_eq!(seen.ci, Some(false));
        assert_eq!(seen.topics, None);
        let mut gone = Script {
            answers: vec![(404, Value::Null)],
            asked: vec![],
        };
        assert!(observe(&mut gone, &target()).unwrap().is_none());
    }

    #[test]
    fn one_put_carries_the_fields_and_what_bitbucket_cannot_hold_is_skipped() {
        let change = |field, to: Value| Change {
            field,
            from: Value::Null,
            to,
        };
        let changes = [
            change(Field::Visibility, json!("private")),
            change(Field::Description, json!("d")),
            change(Field::Topics, json!(["x"])),
            change(Field::Archive, json!(true)),
        ];
        let mut io = Script {
            answers: vec![(200, json!({}))],
            asked: vec![],
        };
        let done = apply(&mut io, &target(), &primary(), &changes).unwrap();
        assert_eq!(io.asked.len(), 1);
        assert_eq!(
            io.asked[0].2,
            Some(json!({"is_private":true,"fork_policy":"no_public_forks","description":"d"}))
        );
        assert_eq!(
            done.iter().filter(|a| a.outcome == "skipped").count(),
            2,
            "topics and archive"
        );
    }

    #[test]
    fn pipelines_are_switched_off_with_a_confirmed_answer() {
        let changes = [Change {
            field: Field::Ci,
            from: json!("on"),
            to: json!("off"),
        }];
        let mut io = Script {
            answers: vec![(200, json!({"enabled":true}))],
            asked: vec![],
        };
        assert!(apply(&mut io, &target(), &primary(), &changes).is_err());
    }

    #[test]
    fn protection_needs_a_push_and_a_merge_restriction() {
        let mut io = Script {
            answers: vec![(
                200,
                json!({"values":[{"kind":"push"},{"kind":"restrict_merges"}]}),
            )],
            asked: vec![],
        };
        assert_eq!(protection(&mut io, &target()).unwrap(), Protection::Locked);
        let mut io = Script {
            answers: vec![(200, json!({"values":[]}))],
            asked: vec![],
        };
        assert_eq!(protection(&mut io, &target()).unwrap(), Protection::Missing);
        let mut io = Script {
            answers: vec![(200, json!({"values":[{"kind":"push"}]}))],
            asked: vec![],
        };
        assert!(matches!(
            protection(&mut io, &target()).unwrap(),
            Protection::Restricted { .. }
        ));
    }
}
