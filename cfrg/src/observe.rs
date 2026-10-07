//! Observe: read-only facts about a repository that a caller acts on, answered
//! by the forge: the head commit of a branch and the statuses reported for one
//! exact commit. Nothing is written and nothing is kept.
use crate::{land::commit_id, status::State, Result};
use serde_json::{json, Value};

/// The forge side of observe. Implemented once per adapter.
pub trait Observe {
    /// Head commit of `branch`; `None` when the branch does not exist.
    fn branch_head(&mut self, branch: &str) -> Result<Option<String>>;
    /// Latest status per context for one exact commit.
    fn commit_statuses(&mut self, commit: &str) -> Result<Vec<(String, State)>>;
}

/// The single output line of `cfrg observe head`.
pub fn head_line(repository: &str, branch: &str, head: Option<&str>) -> Value {
    json!({"repository": repository, "branch": branch, "head": head})
}

/// The single output line of `cfrg observe status`.
pub fn status_line(repository: &str, commit: &str, statuses: &[(String, State)]) -> Result<Value> {
    commit_id(commit)?;
    let rows: Vec<Value> = statuses
        .iter()
        .map(|(context, state)| json!({"context": context, "state": state}))
        .collect();
    Ok(json!({"repository": repository, "commit": commit, "statuses": rows}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_have_one_flat_shape() {
        assert_eq!(
            head_line("o/n", "main", Some("abc")),
            json!({"repository": "o/n", "branch": "main", "head": "abc"})
        );
        assert_eq!(
            head_line("o/n", "gone", None),
            json!({"repository": "o/n", "branch": "gone", "head": null})
        );
        let commit = "a".repeat(40);
        assert_eq!(
            status_line(
                "o/n",
                &commit,
                &[
                    ("ccid/verdict".into(), State::Success),
                    ("ci/crow/build".into(), State::Pending),
                ]
            )
            .unwrap(),
            json!({"repository": "o/n", "commit": commit, "statuses": [
                {"context": "ccid/verdict", "state": "success"},
                {"context": "ci/crow/build", "state": "pending"},
            ]})
        );
        assert!(status_line("o/n", "abc", &[]).is_err());
    }
}
