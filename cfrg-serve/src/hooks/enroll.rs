//! `/hooks/enroll`: a downstream declares (or stops declaring) its upstream, on the forge's push event.
//!
//! Forgejo system hook for `push`. Accepts a push to `refs/heads/main` whose commits list the repository's ROOT file
//! `ccvl-downstream.json` as added, modified or removed (a path below a directory does not count). The command gets
//! `OWNER/REPO AFTER`; it enrols the repository (`hook_commands.enroll`). Forgejo lists only the latest commits of a very
//! large push; such a declaration is repaired by the next change of the file or by a hand run of the enrollment tool.
use super::{event, valid_repo, Job};
use http::HeaderMap;
use serde_json::Value;

pub const PATH: &str = "/hooks/enroll";

const DECLARATION: &str = "ccvl-downstream.json";

fn lists_declaration(commit: &Value) -> bool {
    ["added", "modified", "removed"].iter().any(|list| {
        commit[*list]
            .as_array()
            .is_some_and(|paths| paths.iter().any(|p| p.as_str() == Some(DECLARATION)))
    })
}

pub fn plan(headers: &HeaderMap, payload: &Value) -> Option<Job> {
    if event(headers) != "push" || payload["ref"].as_str() != Some("refs/heads/main") {
        return None;
    }
    let repo = payload["repository"]["full_name"].as_str()?;
    let after = payload["after"].as_str()?;
    if !valid_repo(repo)
        || after.len() != 40
        || !after
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let listed = payload["commits"]
        .as_array()
        .is_some_and(|commits| commits.iter().any(lists_declaration))
        || lists_declaration(&payload["head_commit"]);
    listed.then(|| Job {
        command: "enroll",
        key: repo.to_owned(),
        args: vec![repo.to_owned(), after.to_owned()],
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

    fn push(reference: &str, commits: Value) -> Value {
        json!({"ref": reference, "after": "a".repeat(40), "repository": {"full_name": "o/r"}, "commits": commits})
    }

    #[test]
    fn a_change_of_the_root_declaration_on_main_enrols() {
        for list in ["added", "modified", "removed"] {
            let p = push(
                "refs/heads/main",
                json!([{ "added": [], "modified": ["README.md"], "removed": [], list: ["ccvl-downstream.json"] }]),
            );
            let job = plan(&h("push"), &p).unwrap();
            assert_eq!(job.args, ["o/r".to_owned(), "a".repeat(40)]);
        }
        let head = json!({"ref": "refs/heads/main", "after": "b".repeat(40), "repository": {"full_name": "o/r"}, "commits": [], "head_commit": {"modified": ["ccvl-downstream.json"]}});
        assert!(plan(&h("push"), &head).is_some());
    }

    #[test]
    fn everything_else_is_ignored() {
        let declared = json!([{ "modified": ["ccvl-downstream.json"] }]);
        assert!(plan(&h("push"), &push("refs/heads/dev", declared.clone())).is_none());
        assert!(plan(&h("release"), &push("refs/heads/main", declared)).is_none());
        assert!(plan(
            &h("push"),
            &push(
                "refs/heads/main",
                json!([{ "modified": ["sub/ccvl-downstream.json", "ccvl-downstream.json.bak"] }])
            )
        )
        .is_none());
        let mut bad = push(
            "refs/heads/main",
            json!([{ "modified": ["ccvl-downstream.json"] }]),
        );
        bad["after"] = json!("XYZ");
        assert!(plan(&h("push"), &bad).is_none());
    }
}
