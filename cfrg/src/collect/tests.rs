use super::*;
use cqlt::{Policy, Severity};
use serde_json::json;
use std::collections::BTreeMap;

struct Fixture {
    responses: BTreeMap<String, Value>,
}
impl Transport for Fixture {
    fn get(&self, path: &str) -> Result<Option<Value>> {
        Ok(self.responses.get(path).cloned())
    }
}

/// Test dialect mirroring the GitHub shapes in the fixture below. Real
/// dialects live in `cghb`/`cfgj` and are tested there; the cross-dialect
/// equivalence test lives in `cfrg-cli`.
struct StubSource;
impl EvidenceSource for StubSource {
    fn source_label(&self) -> &'static str {
        "Stub"
    }

    fn default_api_base(&self) -> Option<&'static str> {
        Some("https://api.github.com")
    }
    fn prefers_gh_login(&self) -> bool {
        false
    }
    fn page_param(&self) -> &'static str {
        "per_page"
    }
    fn scope_login_field(&self) -> &'static str {
        "login"
    }
    fn repos_path(&self, login: &str) -> String {
        format!("orgs/{login}/repos?type=all")
    }
    fn profile_repo(&self) -> &'static str {
        ".github"
    }
    fn repo_homepage_field(&self) -> &'static str {
        "homepage"
    }
    fn org_name_field(&self) -> &'static str {
        "name"
    }
    fn org_website_field(&self) -> &'static str {
        "blog"
    }
    fn branch_commit_field(&self) -> &'static str {
        "sha"
    }
    fn repo_topics(
        &self,
        _transport: &dyn Transport,
        _path: &str,
        row: &Value,
    ) -> Result<Vec<String>> {
        let rows = match row.get("topics").and_then(Value::as_array) {
            Some(rows) => rows,
            None => return Err("Missing topics array".to_owned().into()),
        };
        let mut topics = Vec::new();
        for value in rows {
            match value.as_str() {
                Some(topic) => topics.push(topic.to_owned()),
                None => return Err("Invalid topic".to_owned().into()),
            }
        }
        Ok(topics)
    }
    fn repo_readme(
        &self,
        transport: &dyn Transport,
        path: &str,
        revision: &str,
        _root: &[Value],
    ) -> Result<Document> {
        match transport.get(&format!("{path}/readme?ref={revision}"))? {
            Some(value) => Ok(Document::Present {
                path: text(&value, "path")?.into(),
                bytes: value
                    .get("size")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "Missing README size".to_owned())?,
            }),
            None => Ok(Document::Missing),
        }
    }
    fn org_profile(
        &self,
        transport: &dyn Transport,
        path: &str,
        revision: &str,
        root: Vec<Value>,
    ) -> Result<Document> {
        if root
            .iter()
            .any(|e| e["name"] == "profile" && e["type"] == "dir")
        {
            let entries = directory(
                transport,
                &format!("{path}/contents/profile?ref={revision}"),
            )?;
            let entries = entries
                .into_iter()
                .filter(|e| e["name"] == "README.md")
                .collect::<Vec<_>>();
            listed_document(&entries, "profile/", &["README"])
        } else {
            Ok(Document::Missing)
        }
    }
}

static STUB: StubSource = StubSource;

fn fixture() -> Fixture {
    let mut responses = BTreeMap::new();
    responses.insert(
        "user/orgs?per_page=100&page=1".into(),
        json!([{ "id":1,"login":"example","name":"example" }]),
    );
    responses.insert("user/orgs?per_page=100&page=2".into(), json!([]));
    responses.insert("orgs/example".into(),json!({"name":"Example","full_name":"Example","description":"Tools for authors","blog":"https://example.org","website":"https://example.org"}));
    let prefix = "orgs/example/repos?type=all&";
    responses.insert(format!("{prefix}per_page=100&page=1"),json!([{"id":2,"name":"editor","full_name":"example/editor","description":"An editor for authors","homepage":"https://example.org/editor","website":"https://example.org/editor","private":false,"fork":false,"archived":false,"topics":["editing"],"default_branch":"main"}]));
    responses.insert(format!("{prefix}per_page=100&page=2"), json!([]));
    let revision = "a".repeat(40);
    responses.insert(
        "repos/example/editor/branches/main".into(),
        json!({"commit":{"sha":revision,"id":revision}}),
    );
    responses.insert(
        "repos/example/editor/topics".into(),
        json!({"topics":["editing"]}),
    );
    responses.insert(
        format!("repos/example/editor/contents?ref={revision}"),
        json!([
            {"name":"README.md","type":"file","size":80},
            {"name":"LICENSE-MIT","type":"file","size":1000}
        ]),
    );
    responses.insert(
        format!("repos/example/editor/readme?ref={revision}"),
        json!({"path":"README.md","size":80}),
    );
    Fixture { responses }
}

fn snapshot() -> Snapshot {
    collect(
        &fixture(),
        &STUB,
        vec![],
        "test:https://forge.example/api".into(),
    )
    .unwrap()
}

#[test]
fn collected_evidence_evaluates_through_policy() {
    let collected = snapshot();
    assert!(collected.complete);
    assert_eq!(collected.organizations.len(), 1);
    assert_eq!(collected.organizations[0].repositories.len(), 1);
    let report = cqlt::evaluate(&collected, &Policy::default()).unwrap();
    assert_ne!(report.exit_code(Severity::Error), 2);
}

#[test]
fn capped_short_pages_do_not_truncate_inventory() {
    let mut api = fixture();
    api.responses.insert(
        "user/orgs?per_page=100&page=2".into(),
        json!([{"id":3,"login":"second"}]),
    );
    api.responses
        .insert("user/orgs?per_page=100&page=3".into(), json!([]));
    assert_eq!(pages(&api, &STUB, "user/orgs").unwrap().len(), 2);
    let snapshot = collect(&api, &STUB, vec![], "test:x".into()).unwrap();
    assert!(!snapshot.complete);
    assert_eq!(
        cqlt::evaluate(&snapshot, &Policy::default())
            .unwrap()
            .exit_code(Severity::Error),
        2
    );
}

#[test]
fn repeated_pages_and_missing_pages_are_errors() {
    let mut api = fixture();
    let page = api.responses["user/orgs?per_page=100&page=1"].clone();
    api.responses
        .insert("user/orgs?per_page=100&page=2".into(), page);
    assert!(pages(&api, &STUB, "user/orgs").is_err());
    api.responses.remove("user/orgs?per_page=100&page=2");
    assert!(pages(&api, &STUB, "user/orgs").is_err());
}

#[test]
fn unreadable_contents_stays_unknown_instead_of_missing() {
    let mut api = fixture();
    api.responses.remove(&format!(
        "repos/example/editor/contents?ref={}",
        "a".repeat(40)
    ));
    let snapshot = collect(&api, &STUB, vec![], "test:x".into()).unwrap();
    assert!(!snapshot.complete);
    assert!(matches!(
        snapshot.organizations[0].repositories[0].readme,
        Document::Unknown { .. }
    ));
    assert_eq!(
        cqlt::evaluate(&snapshot, &Policy::default())
            .unwrap()
            .exit_code(Severity::Error),
        2
    );
}

#[test]
fn api_urls_and_path_components_do_not_leak_credentials_or_change_routes() {
    for url in [
        "http://forge.example",
        "https://user:secret@forge.example",
        "https://forge.example?token=secret",
        "https:///bad",
        "https://forge.example\n",
    ] {
        assert!(validate_base(url).is_err());
    }
    assert!(validate_base("https://forge.example/api/v1").is_ok());
    assert_eq!(component("feature/a?b#c"), "feature%2Fa%3Fb%23c");
}

#[test]
fn license_variants_count_but_symlinks_and_empty_documents_do_not() {
    let rows = vec![
        json!({"name":"LICENSE-MIT","type":"file","size":50}),
        json!({"name":"LICENSE","type":"symlink","size":15}),
    ];
    assert!(matches!(
        listed_document(&rows, "", &["LICENSE"]).unwrap(),
        Document::Present { bytes: 50, .. }
    ));
    assert!(matches!(
        listed_document(
            &[json!({"name":"README.md","type":"file","size":0})],
            "",
            &["README"]
        )
        .unwrap(),
        Document::Missing
    ));
}

#[test]
fn snapshot_writer_keeps_private_permissions_and_roundtrips() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("snapshot.json");
    let snapshot = snapshot();
    write_snapshot(&path, &snapshot).unwrap();
    let loaded: Snapshot = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(loaded.scope, vec!["example"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    }
}
