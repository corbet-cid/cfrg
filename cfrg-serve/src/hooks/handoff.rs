//! `/hooks/handoff`: the release hand-off, on the forge's own events (one Forgejo system hook for `release` and `create`).
//!
//! * event `release`, action `published`, not a draft: a code release. The command gets `OWNER/REPO TAG`.
//! * event `create`, `ref_type` `tag`, a tag named exactly `aur/<pkgver>-<pkgrel>` (the full ref `refs/tags/...` or the bare
//!   name) made by the sender the policy names (`julian-corbet`): a packaging-only release has no promotion and so no `release`
//!   event; the tag is the event. The command gets `OWNER/REPO aur/<pkgver>-<pkgrel>` (the prefix `refs/tags/` is removed).
//!
//! What the command does with it is configuration (`hook_commands.handoff`): the code release also goes to the downstream
//! dispatch, both kinds go to the host that holds the registry keys. A packaging tag never reaches the downstream dispatch.
use super::{event, valid_repo, Job};
use http::HeaderMap;
use serde_json::Value;

pub const PATH: &str = "/hooks/handoff";

/// The only sender whose packaging tag counts: a workflow job token or a pull mirror also creates tags, and a packaging release is a deliberate act of the owner.
const PACKAGING_SENDER: &str = "julian-corbet";

fn valid_tag(t: &str) -> bool {
    let b = t.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'+' | b'-'))
}

/// `aur/<pkgver>-<pkgrel>`: pkgver starts with a digit (letters and dots after it), pkgrel is 1 to 10 digits.
fn packaging_tag(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("aur/") else {
        return false;
    };
    let Some((ver, rel)) = rest.rsplit_once('-') else {
        return false;
    };
    ver.len() <= 111
        && ver.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && ver.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.')
        && (1..=10).contains(&rel.len())
        && rel.bytes().all(|c| c.is_ascii_digit())
}

pub fn plan(headers: &HeaderMap, payload: &Value) -> Option<Job> {
    let repo = payload["repository"]["full_name"].as_str()?;
    if !valid_repo(repo) {
        return None;
    }
    let tag = match event(headers) {
        "release" => {
            if payload["action"].as_str() != Some("published")
                || payload["release"]["draft"].as_bool() != Some(false)
            {
                return None;
            }
            let tag = payload["release"]["tag_name"].as_str()?;
            valid_tag(tag).then_some(tag)?
        }
        "create" => {
            if payload["ref_type"].as_str() != Some("tag")
                || payload["sender"]["login"].as_str() != Some(PACKAGING_SENDER)
            {
                return None;
            }
            let reference = payload["ref"].as_str()?;
            let name = reference.strip_prefix("refs/tags/").unwrap_or(reference);
            packaging_tag(name).then_some(name)?
        }
        _ => return None,
    };
    Some(Job {
        command: "handoff",
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

    fn release(action: &str, draft: bool, tag: &str) -> Value {
        json!({"action": action, "release": {"draft": draft, "tag_name": tag}, "repository": {"full_name": "o/r"}})
    }

    fn create(reference: &str, kind: &str, sender: &str) -> Value {
        json!({"ref": reference, "ref_type": kind, "sender": {"login": sender}, "repository": {"full_name": "o/r"}})
    }

    #[test]
    fn a_published_release_is_handed_off() {
        let job = plan(&h("release"), &release("published", false, "v1.2.3")).unwrap();
        assert_eq!(job.args, ["o/r", "v1.2.3"]);
        assert!(plan(&h("release"), &release("published", true, "v1.2.3")).is_none());
        assert!(plan(&h("release"), &release("updated", false, "v1.2.3")).is_none());
        assert!(plan(&h("release"), &release("published", false, "-bad")).is_none());
    }

    #[test]
    fn a_packaging_tag_of_the_owner_is_handed_off_without_its_ref_prefix() {
        for reference in ["refs/tags/aur/0.2.0-1", "aur/0.2.0-1"] {
            let job = plan(&h("create"), &create(reference, "tag", "julian-corbet")).unwrap();
            assert_eq!(job.args, ["o/r", "aur/0.2.0-1"]);
        }
    }

    #[test]
    fn anything_else_of_a_create_event_is_ignored() {
        let ok = "refs/tags/aur/0.2.0-1";
        assert!(plan(&h("create"), &create(ok, "branch", "julian-corbet")).is_none());
        assert!(plan(&h("create"), &create(ok, "tag", "someone-else")).is_none());
        for bad in [
            "refs/tags/v1.0.0",
            "refs/tags/aur/x1-1",
            "refs/tags/aur/1.0",
            "refs/tags/aur/1.0-",
            "refs/tags/aur/1.0-12345678901",
            "refs/tags/aur/../1.0-1",
            "refs/tags/aur/1.0-1/x",
        ] {
            assert!(
                plan(&h("create"), &create(bad, "tag", "julian-corbet")).is_none(),
                "{bad}"
            );
        }
        assert!(plan(&h("push"), &create(ok, "tag", "julian-corbet")).is_none());
    }
}
