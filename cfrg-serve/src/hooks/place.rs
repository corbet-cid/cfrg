//! `/hooks/place`: a repository is placed (its derived copies created, with their settings) on the forge's own event.
//!
//! Forgejo system webhook events `repository` and `push`. The job is `hook_commands.place` with `--repository OWNER/REPO`
//! appended; the command runs `cfrg native` for that one repository, which derives its row from the placement's
//! `derive` policy (`docs/native-mirrors.md`, "Derived placement"). Idempotent: a repository already covered is only read.
//!
//! * `repository`, any action except `deleted`: created, and whatever Forgejo reports for a visibility change, a rename
//!   or a transfer. The daily run is the catch-up for what no event reported.
//! * `push` that creates the default branch (all-zero `before`): the first commit of a repository that was empty when it
//!   was created, and so skipped then. Every other push is native mirroring's business and plans nothing.
use super::{event, valid_repo, Job};
use http::HeaderMap;
use serde_json::Value;

pub const PATH: &str = "/hooks/place";

fn first_commit(payload: &Value) -> bool {
    let zeros = |s: &str| !s.is_empty() && s.bytes().all(|b| b == b'0');
    let branch = payload["repository"]["default_branch"]
        .as_str()
        .unwrap_or("");
    !branch.is_empty()
        && payload["ref"].as_str() == Some(&format!("refs/heads/{branch}"))
        && payload["before"].as_str().is_some_and(zeros)
        && !payload["after"].as_str().is_some_and(zeros)
}

pub fn plan(headers: &HeaderMap, payload: &Value) -> Option<Job> {
    match event(headers) {
        "repository" if payload["action"].as_str() == Some("deleted") => return None,
        "repository" => {}
        "push" if first_commit(payload) => {}
        _ => return None,
    }
    let repo = payload["repository"]["full_name"].as_str()?;
    valid_repo(repo).then(|| Job {
        command: "place",
        key: repo.to_owned(),
        args: vec!["--repository".into(), repo.to_owned()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn h(e: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert("x-forgejo-event", e.parse().unwrap());
        m
    }

    #[test]
    fn creation_and_every_other_repository_change_place_the_repository() {
        for action in ["created", "edited", "transferred", "renamed", "visibility"] {
            let p = json!({"action": action, "repository": {"full_name": "o/r"}});
            assert_eq!(
                plan(&h("repository"), &p).unwrap().args,
                ["--repository", "o/r"]
            );
        }
        let gone = json!({"action": "deleted", "repository": {"full_name": "o/r"}});
        assert!(plan(&h("repository"), &gone).is_none());
    }

    #[test]
    fn only_the_first_commit_of_the_default_branch_places_on_push() {
        let zero = "0".repeat(40);
        let push = |branch: &str, before: &str| {
            json!({"ref": format!("refs/heads/{branch}"), "before": before, "after": "a".repeat(40),
                   "repository": {"full_name": "o/r", "default_branch": "main"}})
        };
        assert!(plan(&h("push"), &push("main", &zero)).is_some());
        assert!(plan(&h("push"), &push("main", &"b".repeat(40))).is_none());
        assert!(plan(&h("push"), &push("topic", &zero)).is_none());
    }

    #[test]
    fn other_events_and_unsafe_names_do_nothing() {
        let p = json!({"action": "created", "repository": {"full_name": "o/r"}});
        assert!(plan(&h("issues"), &p).is_none());
        assert!(plan(
            &h("repository"),
            &json!({"action": "created", "repository": {"full_name": "../x"}})
        )
        .is_none());
    }
}
