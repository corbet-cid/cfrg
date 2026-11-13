//! `/hooks/lockrefresh`: a published release starts the first-party lock refresh of the repositories that depend on
//! the released crate (the command that runs is yours: `hook_commands.lockrefresh`). A second Forgejo
//! system hook for the `release` event points here, so the release upload route is untouched.
use super::{event, valid_repo, Job};
use http::HeaderMap;
use serde_json::Value;

pub const PATH: &str = "/hooks/lockrefresh";

fn valid_tag(t: &str) -> bool {
    let b = t.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'+' | b'-'))
}

pub fn plan(headers: &HeaderMap, payload: &Value) -> Option<Job> {
    if event(headers) != "release"
        || payload["action"].as_str() != Some("published")
        || payload["release"]["draft"].as_bool() != Some(false)
    {
        return None;
    }
    let repo = payload["repository"]["full_name"].as_str()?;
    let tag = payload["release"]["tag_name"].as_str()?;
    (valid_repo(repo) && valid_tag(tag)).then(|| Job {
        command: "lockrefresh",
        key: format!("{repo}@{tag}"),
        args: vec![repo.to_owned(), tag.to_owned()],
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
    fn published_non_draft_only() {
        let p = |a: &str, d: bool| json!({"action": a, "release": {"draft": d, "tag_name": "v1.2.3"}, "repository": {"full_name": "o/r"}});
        assert_eq!(
            plan(&h("release"), &p("published", false)).unwrap().args,
            ["o/r", "v1.2.3"]
        );
        assert!(plan(&h("release"), &p("published", true)).is_none());
        assert!(plan(&h("release"), &p("updated", false)).is_none());
        assert!(plan(&h("push"), &p("published", false)).is_none());
    }
}
