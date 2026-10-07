//! Forgejo contents: the git tree and blob API, native. One tree request per
//! directory read; a blob is fetched only when the caller does not hold it.
use cfrg::{
    contents::{ContentsSource, Entry, Kind},
    land::{Capability, Support},
    native::{encode, http::Auth},
    Result,
};
use serde_json::Value;

pub const CONTENTS: Capability = Capability {
    support: Support::Native,
    note: "native: git tree and blob API of the repository, one tree request per directory read (paged), blob bytes only for ids the caller does not hold",
};

pub struct Contents;
pub static SOURCE: Contents = Contents;

impl ContentsSource for Contents {
    fn auth(&self) -> Auth {
        Auth::Token
    }

    /// 404 for an unknown repository, 409 for an empty one, and 400
    /// ("sha not found") for a branch that does not exist.
    fn missing(&self, status: u16) -> bool {
        matches!(status, 400 | 404 | 409)
    }

    fn tree_path(&self, repository: &str, tree: &str, page: u32) -> String {
        let base = format!("/api/v1/repos/{repository}/git/trees/{}", encode(tree));
        if page > 1 {
            format!("{base}?page={page}")
        } else {
            base
        }
    }

    fn entries(&self, reply: &Value) -> Result<(Vec<Entry>, bool)> {
        let rows = reply["tree"]
            .as_array()
            .ok_or("Malformed Forgejo tree reply")?;
        let entries = rows
            .iter()
            .filter_map(|row| {
                let kind = match row["type"].as_str()? {
                    "blob" => Kind::Blob,
                    "tree" => Kind::Tree,
                    // Submodules are not files.
                    _ => return None,
                };
                Some(Entry {
                    name: row["path"].as_str()?.to_owned(),
                    id: row["sha"].as_str()?.to_owned(),
                    kind,
                })
            })
            .collect();
        Ok((entries, reply["truncated"] == true))
    }

    fn blob_path(&self, repository: &str, id: &str) -> String {
        format!("/api/v1/repos/{repository}/git/blobs/{id}")
    }

    fn blob_base64(&self, reply: &Value) -> Result<String> {
        if reply["encoding"] != "base64" {
            return Err("Unexpected Forgejo blob encoding".into());
        }
        let packed: String = reply["content"]
            .as_str()
            .ok_or("Malformed Forgejo blob reply")?
            .split_whitespace()
            .collect();
        Ok(packed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths_follow_the_native_api_and_encode_the_branch() {
        assert_eq!(
            SOURCE.tree_path("o/n", "main", 1),
            "/api/v1/repos/o/n/git/trees/main"
        );
        assert_eq!(
            SOURCE.tree_path("o/n", "release/1.0", 2),
            "/api/v1/repos/o/n/git/trees/release%2F1.0?page=2"
        );
        assert_eq!(
            SOURCE.blob_path("o/n", "abc"),
            "/api/v1/repos/o/n/git/blobs/abc"
        );
    }

    #[test]
    fn an_unknown_branch_repository_or_empty_repository_is_missing() {
        for status in [400, 404, 409] {
            assert!(SOURCE.missing(status), "{status}");
        }
        for status in [200, 401, 403, 429, 500] {
            assert!(!SOURCE.missing(status), "{status}");
        }
    }

    #[test]
    fn blobs_and_directories_are_listed_and_truncation_asks_for_more() {
        let reply = json!({"truncated": true, "tree": [
            {"path": "Cargo.lock", "type": "blob", "sha": "a".repeat(40)},
            {"path": "src", "type": "tree", "sha": "b".repeat(40)},
            {"path": "vendor", "type": "commit", "sha": "c".repeat(40)},
        ]});
        let (entries, more) = SOURCE.entries(&reply).unwrap();
        assert_eq!(
            entries,
            vec![
                Entry {
                    name: "Cargo.lock".into(),
                    id: "a".repeat(40),
                    kind: Kind::Blob
                },
                Entry {
                    name: "src".into(),
                    id: "b".repeat(40),
                    kind: Kind::Tree
                },
            ]
        );
        assert!(more);
        assert!(!SOURCE.entries(&json!({"tree": []})).unwrap().1);
        assert!(SOURCE.entries(&json!({})).is_err());
    }

    #[test]
    fn blob_content_is_whitespace_free_base64_only() {
        assert_eq!(
            SOURCE
                .blob_base64(&json!({"encoding": "base64", "content": "Zm9v\nYmFy\n"}))
                .unwrap(),
            "Zm9vYmFy"
        );
        assert!(SOURCE
            .blob_base64(&json!({"encoding": "none", "content": "x"}))
            .is_err());
    }
}
