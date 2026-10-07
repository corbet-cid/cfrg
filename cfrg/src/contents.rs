//! Contents: read named files of a repository branch through the forge API,
//! never by clone.
//!
//! The forge-neutral procedure is here; an adapter supplies the paths and the
//! shapes of its tree and blob replies ([`ContentsSource`]). A blob id is a
//! content address, so a caller that already holds the result for a blob
//! names it in [`Query::known`] and its bytes are not fetched again: a warm
//! read of root files costs one tree request per repository, and one more per
//! directory a wanted path passes through.
//!
//! Reads go through the paced transport with durable rate windows. The first
//! transport failure (a rate window, a refused credential, a server error)
//! stops the run; the remaining targets are reported as not read instead of
//! being asked again, and a rerun starts from the caller's known blobs.
use crate::{
    failure,
    land::branch_name,
    native::{
        self,
        http::{expect, Auth, Request, Transport},
        Endpoint,
    },
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Listing pages read per directory (a directory rarely has more than one).
const MAX_PAGES: u32 = 5;
const MAX_DEPTH: usize = 4;
const MAX_TARGETS: usize = 5000;
const MAX_PATHS: usize = 16;
const MAX_KNOWN: usize = 200_000;

/// One repository branch to read.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// `owner/name`; nested group paths where the forge has them.
    pub repository: String,
    pub branch: String,
}

/// What to read, supplied by the caller as one JSON document.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub targets: Vec<Target>,
    /// Files wanted, as `/`-separated paths below the root (`Cargo.lock`,
    /// `.ci/ccid.toml`).
    pub paths: Vec<String>,
    /// Blob ids whose bytes the caller already holds: listed, never fetched.
    #[serde(default)]
    pub known: BTreeSet<String>,
}

impl Query {
    pub fn validate(&self) -> Result<()> {
        if self.targets.len() > MAX_TARGETS || self.known.len() > MAX_KNOWN {
            return Err(failure("Contents query too large"));
        }
        if self.paths.is_empty() || self.paths.len() > MAX_PATHS {
            return Err(failure("Name between one and sixteen files"));
        }
        for path in &self.paths {
            file_path(path)?;
        }
        for target in &self.targets {
            native::path(&target.repository, true)?;
            branch_name(&target.branch)?;
        }
        for id in &self.known {
            blob_id(id)?;
        }
        Ok(())
    }
}

fn file_path(value: &str) -> Result<()> {
    let parts: Vec<&str> = value.split('/').collect();
    if parts.len() > MAX_DEPTH {
        return Err(failure("Invalid file path"));
    }
    parts.into_iter().try_for_each(file_name)
}

fn file_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    {
        return Err(failure("Invalid file name"));
    }
    Ok(())
}

/// A git blob id: SHA-1 (40) or SHA-256 (64) lowercase hex.
fn blob_id(value: &str) -> Result<()> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(failure("Invalid blob id"));
    }
    Ok(())
}

/// What a directory entry is. Anything else (a submodule, a link) is not
/// listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Blob,
    Tree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub id: String,
    pub kind: Kind,
}

/// The forge side of contents. Implemented once per adapter.
pub trait ContentsSource {
    fn auth(&self) -> Auth;
    /// Whether a listing answered with this status means the branch or the
    /// directory does not exist (as opposed to a failure).
    fn missing(&self, status: u16) -> bool {
        matches!(status, 404 | 409)
    }
    /// Path (below the origin) of one page (from 1) of a listing. `tree` is a
    /// branch name for the root, or the id of a directory.
    fn tree_path(&self, repository: &str, tree: &str, page: u32) -> String;
    /// The blobs and directories of one page, and whether another page follows.
    fn entries(&self, reply: &Value) -> Result<(Vec<Entry>, bool)>;
    fn blob_path(&self, repository: &str, id: &str) -> String;
    /// Base64 of the blob's bytes with all whitespace removed.
    fn blob_base64(&self, reply: &Value) -> Result<String>;
}

/// One wanted file that exists.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct File {
    pub path: String,
    /// Content address of the bytes.
    pub blob: String,
    /// Base64 of the bytes; absent when the caller listed the blob as known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Outcome {
    /// The branch exists; `files` are the wanted paths that exist.
    Found { files: Vec<File> },
    /// The repository, the branch or its tree does not exist (or is empty).
    Absent,
    /// Not read; the message carries no credential and no response body.
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Report {
    pub repository: String,
    pub branch: String,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// A failure of one target. `stop` marks a transport failure: asking the
/// forge again would only repeat it.
struct Fault {
    stop: bool,
    error: String,
}

fn request(source: &dyn ContentsSource, endpoint: &Endpoint, path: String) -> Request {
    Request {
        endpoint: endpoint.clone(),
        auth: source.auth(),
        method: "GET",
        path,
        body: None,
        scope: format!("{}/api", endpoint.origin),
        creation: false,
    }
}

/// Every entry of one listing, page by page; `None` when it does not exist.
fn listing(
    source: &dyn ContentsSource,
    io: &mut dyn Transport,
    endpoint: &Endpoint,
    repository: &str,
    tree: &str,
) -> std::result::Result<Option<Vec<Entry>>, Fault> {
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let response = io
            .send(request(
                source,
                endpoint,
                source.tree_path(repository, tree, page),
            ))
            .map_err(transport)?;
        if source.missing(response.status) {
            if page == 1 {
                return Ok(None);
            }
            break;
        }
        let reply = expect(response, &[200]).map_err(local)?;
        let (entries, more) = source.entries(&reply).map_err(local)?;
        all.extend(entries);
        if !more {
            break;
        }
    }
    Ok(Some(all))
}

fn transport(e: Box<dyn std::error::Error + Send + Sync>) -> Fault {
    Fault {
        stop: true,
        error: e.to_string(),
    }
}

fn local(e: Box<dyn std::error::Error + Send + Sync>) -> Fault {
    Fault {
        stop: false,
        error: e.to_string(),
    }
}

fn read_one(
    source: &dyn ContentsSource,
    io: &mut dyn Transport,
    endpoint: &Endpoint,
    query: &Query,
    target: &Target,
) -> std::result::Result<Outcome, Fault> {
    let Some(root) = listing(source, io, endpoint, &target.repository, &target.branch)? else {
        return Ok(Outcome::Absent);
    };
    // Directory listings read so far, by directory id.
    let mut directories: BTreeMap<String, Option<Vec<Entry>>> = BTreeMap::new();
    let mut files = Vec::new();
    for path in &query.paths {
        let mut entries = root.clone();
        let parts: Vec<&str> = path.split('/').collect();
        let mut found = None;
        for (depth, name) in parts.iter().enumerate() {
            let Some(entry) = entries.iter().find(|e| e.name == *name) else {
                break;
            };
            if depth + 1 == parts.len() {
                found = (entry.kind == Kind::Blob).then(|| entry.id.clone());
                break;
            }
            if entry.kind != Kind::Tree {
                break;
            }
            let id = entry.id.clone();
            if !directories.contains_key(&id) {
                let read = listing(source, io, endpoint, &target.repository, &id)?;
                directories.insert(id.clone(), read);
            }
            match &directories[&id] {
                Some(next) => entries = next.clone(),
                None => break,
            }
        }
        let Some(id) = found else { continue };
        blob_id(&id).map_err(local)?;
        let content = if query.known.contains(&id) {
            None
        } else {
            let response = io
                .send(request(
                    source,
                    endpoint,
                    source.blob_path(&target.repository, &id),
                ))
                .map_err(transport)?;
            let reply = expect(response, &[200]).map_err(local)?;
            Some(source.blob_base64(&reply).map_err(local)?)
        };
        files.push(File {
            path: path.clone(),
            blob: id,
            content,
        });
    }
    Ok(Outcome::Found { files })
}

/// Read every target, handing each report to `emit` as soon as it exists.
/// Returns the number of targets that could not be read.
pub fn read(
    source: &dyn ContentsSource,
    io: &mut dyn Transport,
    endpoint: &Endpoint,
    query: &Query,
    emit: &mut dyn FnMut(&Report) -> Result<()>,
) -> Result<usize> {
    endpoint.validate()?;
    query.validate()?;
    let mut failed = 0;
    let mut stopped = false;
    for target in &query.targets {
        let outcome = if stopped {
            Outcome::Failed {
                error: "not read: an earlier request failed".into(),
            }
        } else {
            match read_one(source, io, endpoint, query, target) {
                Ok(outcome) => outcome,
                Err(fault) => {
                    stopped = fault.stop;
                    Outcome::Failed { error: fault.error }
                }
            }
        };
        if matches!(outcome, Outcome::Failed { .. }) {
            failed += 1;
        }
        emit(&Report {
            repository: target.repository.clone(),
            branch: target.branch.clone(),
            outcome,
        })?;
    }
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::http::Response;
    use serde_json::json;
    use std::collections::BTreeMap;

    /// Scripted forge: path -> (status, body); records every request path.
    struct Fake {
        replies: BTreeMap<String, (u16, Value)>,
        seen: Vec<String>,
        fail_on: Option<String>,
    }
    impl Transport for Fake {
        fn send(&mut self, request: Request) -> Result<Response> {
            assert_eq!(request.method, "GET");
            self.seen.push(request.path.clone());
            if self.fail_on.as_deref() == Some(request.path.as_str()) {
                return Err(failure("Native API HTTP 429; stopped without retry"));
            }
            let (status, body) = self
                .replies
                .get(&request.path)
                .cloned()
                .unwrap_or((404, Value::Null));
            Ok(Response { status, body })
        }
    }

    /// Dialect used by the tests: rows carry `kind` ("blob" or "tree").
    struct Dialect;
    impl ContentsSource for Dialect {
        fn auth(&self) -> Auth {
            Auth::Token
        }
        fn tree_path(&self, repository: &str, tree: &str, page: u32) -> String {
            format!("/t/{repository}/{tree}/{page}")
        }
        fn entries(&self, reply: &Value) -> Result<(Vec<Entry>, bool)> {
            let rows = reply["rows"].as_array().ok_or_else(|| failure("shape"))?;
            Ok((
                rows.iter()
                    .map(|r| Entry {
                        name: r["name"].as_str().unwrap_or_default().to_owned(),
                        id: r["id"].as_str().unwrap_or_default().to_owned(),
                        kind: if r["kind"] == "tree" {
                            Kind::Tree
                        } else {
                            Kind::Blob
                        },
                    })
                    .collect(),
                reply["more"] == true,
            ))
        }
        fn blob_path(&self, repository: &str, id: &str) -> String {
            format!("/b/{repository}/{id}")
        }
        fn blob_base64(&self, reply: &Value) -> Result<String> {
            Ok(reply["content"]
                .as_str()
                .ok_or_else(|| failure("shape"))?
                .to_owned())
        }
    }

    fn id(c: char) -> String {
        c.to_string().repeat(40)
    }
    fn endpoint() -> Endpoint {
        Endpoint {
            origin: "https://forge.example".into(),
            token_env: "TOKEN".into(),
        }
    }
    fn query(targets: &[(&str, &str)], known: &[String]) -> Query {
        Query {
            targets: targets
                .iter()
                .map(|(r, b)| Target {
                    repository: (*r).into(),
                    branch: (*b).into(),
                })
                .collect(),
            paths: vec!["Cargo.lock".into(), "flake.lock".into()],
            known: known.iter().cloned().collect(),
        }
    }
    fn run(fake: &mut Fake, query: &Query) -> (Vec<Report>, usize) {
        let mut reports = Vec::new();
        let failed = read(&Dialect, fake, &endpoint(), query, &mut |r| {
            reports.push(r.clone());
            Ok(())
        })
        .unwrap();
        (reports, failed)
    }
    fn fake(replies: Vec<(String, u16, Value)>) -> Fake {
        Fake {
            replies: replies.into_iter().map(|(p, s, b)| (p, (s, b))).collect(),
            seen: vec![],
            fail_on: None,
        }
    }

    #[test]
    fn known_blobs_are_listed_and_never_fetched() {
        let (cargo, flake) = (id('a'), id('b'));
        let mut forge = fake(vec![
            (
                "/t/o/n/main/1".into(),
                200,
                json!({"rows": [
                    {"name": "README.md", "id": id('c')},
                    {"name": "Cargo.lock", "id": cargo},
                    {"name": "flake.lock", "id": flake},
                ]}),
            ),
            (
                format!("/b/o/n/{flake}"),
                200,
                json!({"content": "ZmxvY2s="}),
            ),
        ]);
        let (reports, failed) = run(
            &mut forge,
            &query(&[("o/n", "main")], std::slice::from_ref(&cargo)),
        );
        assert_eq!(failed, 0);
        assert_eq!(
            reports[0].outcome,
            Outcome::Found {
                files: vec![
                    File {
                        path: "Cargo.lock".into(),
                        blob: cargo,
                        content: None
                    },
                    File {
                        path: "flake.lock".into(),
                        blob: flake.clone(),
                        content: Some("ZmxvY2s=".into())
                    },
                ]
            }
        );
        // One tree request and exactly one blob request.
        assert_eq!(forge.seen.len(), 2);
    }

    #[test]
    fn missing_branch_is_absent_and_pages_are_followed() {
        let lock = id('d');
        let mut forge = fake(vec![
            (
                "/t/o/paged/main/1".into(),
                200,
                json!({"rows": [{"name": "a", "id": id('1')}], "more": true}),
            ),
            (
                "/t/o/paged/main/2".into(),
                200,
                json!({"rows": [{"name": "Cargo.lock", "id": lock}]}),
            ),
            (
                format!("/b/o/paged/{lock}"),
                200,
                json!({"content": "eA=="}),
            ),
            ("/t/o/empty/main/1".into(), 409, Value::Null),
        ]);
        let (reports, failed) = run(
            &mut forge,
            &query(
                &[("o/empty", "main"), ("o/gone", "main"), ("o/paged", "main")],
                &[],
            ),
        );
        assert_eq!(failed, 0);
        assert_eq!(reports[0].outcome, Outcome::Absent);
        assert_eq!(reports[1].outcome, Outcome::Absent);
        assert!(matches!(&reports[2].outcome, Outcome::Found { files } if files.len() == 1));
    }

    #[test]
    fn a_transport_failure_stops_the_run_without_asking_again() {
        let mut forge = fake(vec![("/t/o/b/main/1".into(), 200, json!({"rows": []}))]);
        forge.fail_on = Some("/t/o/a/main/1".into());
        let (reports, failed) = run(
            &mut forge,
            &query(&[("o/a", "main"), ("o/b", "main"), ("o/c", "main")], &[]),
        );
        assert_eq!(failed, 3);
        assert_eq!(forge.seen, vec!["/t/o/a/main/1"]);
        assert!(
            matches!(&reports[1].outcome, Outcome::Failed { error } if error.contains("earlier request"))
        );
    }

    #[test]
    fn nested_paths_walk_directories_and_read_each_directory_once() {
        let (a, b, dir) = (id('a'), id('b'), id('d'));
        let mut forge = fake(vec![
            (
                "/t/o/n/main/1".into(),
                200,
                json!({"rows": [
                    {"name": ".ci", "id": dir, "kind": "tree"},
                    {"name": "ci", "id": id('e')},
                ]}),
            ),
            (
                format!("/t/o/n/{dir}/1"),
                200,
                json!({"rows": [
                    {"name": "ccid.toml", "id": a},
                    {"name": "jobs.json", "id": b},
                    {"name": "sub", "id": id('f'), "kind": "tree"},
                ]}),
            ),
            (format!("/b/o/n/{a}"), 200, json!({"content": "YQ=="})),
            (format!("/b/o/n/{b}"), 200, json!({"content": "Yg=="})),
        ]);
        let mut wanted = query(&[("o/n", "main")], &[]);
        wanted.paths = vec![
            ".ci/ccid.toml".into(),
            ".ci/jobs.json".into(),
            ".ci/sub".into(),
            "ci/ccid.toml".into(),
            ".ci/missing/x".into(),
            "nothing/here".into(),
        ];
        let (reports, failed) = run(&mut forge, &wanted);
        assert_eq!(failed, 0);
        let Outcome::Found { files } = &reports[0].outcome else {
            panic!("{:?}", reports[0]);
        };
        let names: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
        // A directory is not a file, a blob is not a directory, absent parts are skipped.
        assert_eq!(names, [".ci/ccid.toml", ".ci/jobs.json"]);
        assert_eq!(files[0].content.as_deref(), Some("YQ=="));
        // Root once, `.ci` once (for all its paths), two blobs.
        assert_eq!(forge.seen.len(), 4, "{:?}", forge.seen);
    }

    #[test]
    fn a_bad_reply_fails_one_target_only() {
        let mut forge = fake(vec![
            ("/t/o/a/main/1".into(), 200, json!({"nope": 1})),
            ("/t/o/b/main/1".into(), 200, json!({"rows": []})),
        ]);
        let (reports, failed) = run(&mut forge, &query(&[("o/a", "main"), ("o/b", "main")], &[]));
        assert_eq!(failed, 1);
        assert!(matches!(reports[0].outcome, Outcome::Failed { .. }));
        assert_eq!(reports[1].outcome, Outcome::Found { files: vec![] });
    }

    #[test]
    fn requests_that_could_change_the_path_or_the_credential_are_refused() {
        let mut bad = query(&[("o/n", "main")], &[]);
        for path in [
            "../Cargo.lock",
            "a//b",
            "/a",
            "a/",
            "a/./b",
            "a/b/c/d/e",
            "a b",
        ] {
            bad.paths = vec![path.into()];
            assert!(bad.validate().is_err(), "{path}");
        }
        for branch in ["", "-x", ".x", "a..b", "/x", "x/", "a b", "a?b=c", "a%2fb"] {
            assert!(
                query(&[("o/n", branch)], &[]).validate().is_err(),
                "{branch}"
            );
        }
        for repository in ["o", "o/n/../x", "o/n?x=1", "o/%2e%2e"] {
            assert!(
                query(&[(repository, "main")], &[]).validate().is_err(),
                "{repository}"
            );
        }
        assert!(query(&[("o/n", "main")], &["xyz".into()])
            .validate()
            .is_err());
        assert!(query(&[("g/s/n", "release/1.0")], &[id('f')])
            .validate()
            .is_ok());
    }

    #[test]
    fn report_lines_have_one_flat_shape() {
        let report = Report {
            repository: "o/n".into(),
            branch: "main".into(),
            outcome: Outcome::Found {
                files: vec![File {
                    path: "Cargo.lock".into(),
                    blob: id('a'),
                    content: None,
                }],
            },
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            json!({"repository":"o/n","branch":"main","state":"found",
                   "files":[{"path":"Cargo.lock","blob":id('a')}]})
        );
        assert_eq!(
            serde_json::to_value(Report {
                repository: "o/n".into(),
                branch: "main".into(),
                outcome: Outcome::Absent
            })
            .unwrap(),
            json!({"repository":"o/n","branch":"main","state":"absent"})
        );
    }
}
