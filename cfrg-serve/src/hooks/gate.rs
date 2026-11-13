//! `/hooks/gate`: the GitHub write gate on the forge's own push event instead of a 15-minute timer.
//!
//! Forgejo system webhook event `push`. The job is the configured `gate` command with the repository appended: the
//! command enqueues that repository into the gate's queue and runs ONE paced `cfrg gate flush`. The gate itself
//! (budgets of `cfrg gate`, its ledger, the paused directories, the kill switch) decides what is written, so a burst of
//! pushes queues at most one waiting job per repository and can never exceed the budget; a repository the gate does not
//! list is refused by `enqueue` and costs nothing. Whether the flush really writes is the gate configuration's
//! `execute` flag, not this module. Nothing is retried (R30): the next push, or the daily run, repairs a failure.
use super::{event, valid_repo, Job};
use http::HeaderMap;
use serde_json::Value;

pub const PATH: &str = "/hooks/gate";

pub fn plan(headers: &HeaderMap, payload: &Value) -> Option<Job> {
    if event(headers) != "push" {
        return None;
    }
    // A push that deletes its ref carries the all-zero "after" commit: nothing to publish.
    if payload["after"]
        .as_str()
        .is_some_and(|a| a.bytes().all(|b| b == b'0'))
    {
        return None;
    }
    let repo = payload["repository"]["full_name"].as_str()?;
    valid_repo(repo).then(|| Job {
        command: "gate",
        key: format!("gate:{repo}"),
        args: vec![repo.to_owned()],
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
    fn a_push_plans_a_gate_job() {
        let p = json!({"after": "abc123", "repository": {"full_name": "o/r"}});
        let job = plan(&h("push"), &p).unwrap();
        assert_eq!(
            (job.command, job.key.as_str(), job.args),
            ("gate", "gate:o/r", vec!["o/r".to_owned()])
        );
    }

    #[test]
    fn deletes_other_events_and_bad_names_plan_nothing() {
        let del = json!({"after": "0000000000000000000000000000000000000000", "repository": {"full_name": "o/r"}});
        assert!(plan(&h("push"), &del).is_none());
        let p = json!({"after": "abc", "repository": {"full_name": "o/r"}});
        assert!(plan(&h("release"), &p).is_none());
        let bad = json!({"after": "abc", "repository": {"full_name": "../x"}});
        assert!(plan(&h("push"), &bad).is_none());
    }
}
