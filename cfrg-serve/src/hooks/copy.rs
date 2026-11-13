//! `/hooks/copy`: the git copy of a repository, on the forge's own event instead of an hourly timer.
//!
//! Forgejo system webhook events `repository` (action `created`) and `push`. The job is `cfrg native --provider copy
//! --repository OWNER/REPO --apply`: idempotent, a repository already covered is only read. A repository deleted or
//! renamed is the daily drift run's business.
use super::{event, valid_repo, Job};
use http::HeaderMap;
use serde_json::Value;

pub const PATH: &str = "/hooks/copy";

pub fn plan(headers: &HeaderMap, payload: &Value) -> Option<Job> {
    match event(headers) {
        "repository" if payload["action"].as_str() != Some("created") => return None,
        "repository" | "push" => {}
        _ => return None,
    }
    let repo = payload["repository"]["full_name"].as_str()?;
    valid_repo(repo).then(|| Job {
        command: "copy",
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
    fn created_and_push_plan_a_copy() {
        let p = json!({"action": "created", "repository": {"full_name": "o/r"}});
        assert_eq!(
            plan(&h("repository"), &p).unwrap().args,
            ["--repository", "o/r"]
        );
        assert!(plan(&h("push"), &json!({"repository": {"full_name": "o/r"}})).is_some());
    }

    #[test]
    fn other_actions_and_events_do_nothing() {
        let p = json!({"action": "deleted", "repository": {"full_name": "o/r"}});
        assert!(plan(&h("repository"), &p).is_none());
        assert!(plan(&h("issues"), &p).is_none());
        assert!(plan(&h("push"), &json!({"repository": {"full_name": "../x"}})).is_none());
    }
}
