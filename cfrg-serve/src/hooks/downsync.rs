//! `/hooks/downsync`: a green run of `ci.yml` on `main` fans out to the downstreams that follow the repository
//! (the command that runs is yours: `hook_commands.downsync`). Accepts exactly this shape: event `action_run_success`, `action` success, workflow `ci.yml`, ref `main`, a 40-hex commit.
use super::{event, valid_repo, Job};
use http::HeaderMap;
use serde_json::Value;

pub const PATH: &str = "/hooks/downsync";

pub fn plan(headers: &HeaderMap, payload: &Value) -> Option<Job> {
    if event(headers) != "action_run_success"
        || payload["action"].as_str() != Some("success")
        || payload["run"]["workflow_id"].as_str() != Some("ci.yml")
        || payload["run"]["prettyref"].as_str() != Some("main")
    {
        return None;
    }
    let repo = payload["run"]["repository"]["full_name"].as_str()?;
    let sha = payload["run"]["commit_sha"].as_str()?;
    if !valid_repo(repo)
        || sha.len() != 40
        || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    Some(Job {
        command: "downsync",
        key: format!("{repo}@{sha}"),
        // REPOSITORY COMMIT WORKFLOW REF ACTION: the five arguments of the configured command.
        args: [repo, sha, "ci.yml", "main", "success"]
            .map(str::to_owned)
            .to_vec(),
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

    fn body(wf: &str, r: &str, sha: &str) -> Value {
        json!({"action": "success", "run": {"workflow_id": wf, "prettyref": r, "commit_sha": sha,
                                             "repository": {"full_name": "o/r"}}})
    }

    #[test]
    fn green_main_ci_dispatches() {
        let sha = "a".repeat(40);
        let job = plan(&h("action_run_success"), &body("ci.yml", "main", &sha)).unwrap();
        assert_eq!(job.args, ["o/r", sha.as_str(), "ci.yml", "main", "success"]);
    }

    #[test]
    fn everything_else_is_ignored() {
        let sha = "a".repeat(40);
        assert!(plan(&h("action_run_success"), &body("other.yml", "main", &sha)).is_none());
        assert!(plan(&h("action_run_success"), &body("ci.yml", "dev", &sha)).is_none());
        assert!(plan(&h("action_run_success"), &body("ci.yml", "main", "XYZ")).is_none());
        assert!(plan(&h("push"), &body("ci.yml", "main", &sha)).is_none());
    }
}
