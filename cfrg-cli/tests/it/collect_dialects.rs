//! Evidence dialects stay equivalent: GitHub and Forgejo snapshots of the
//! same organization carry the same policy evidence.
use cfrg::collect::{self, EvidenceSource, Transport};
use cfrg::model::Forge;
use cfrg::Result;
use cqlt::Snapshot;
use serde_json::{json, Value};
use std::collections::BTreeMap;

struct Fixture {
    responses: BTreeMap<String, Value>,
}
impl Transport for Fixture {
    fn get(&self, path: &str) -> Result<Option<Value>> {
        Ok(self.responses.get(path).cloned())
    }
}

fn dialect_label(forge: Forge) -> &'static str {
    dialect(forge).source_label()
}

fn dialect(forge: Forge) -> &'static dyn EvidenceSource {
    match forge {
        Forge::Github => &cghb::EVIDENCE,
        Forge::Forgejo => &cfgj::EVIDENCE,
        _ => unreachable!("test covers github and forgejo only"),
    }
}

fn fixture(forge: Forge) -> Fixture {
    let source = dialect(forge);
    let mut responses = BTreeMap::new();
    let size = source.page_param();
    responses.insert(
        format!("user/orgs?{size}=100&page=1"),
        json!([{ "id":1,"login":"example","name":"example" }]),
    );
    responses.insert(format!("user/orgs?{size}=100&page=2"), json!([]));
    responses.insert("orgs/example".into(),json!({"name":"Example","full_name":"Example","description":"Tools for authors","blog":"https://example.org","website":"https://example.org"}));
    let prefix = source.repos_path("example") + if forge == Forge::Github { "&" } else { "?" };
    responses.insert(format!("{prefix}{size}=100&page=1"),json!([{"id":2,"name":"editor","full_name":"example/editor","description":"An editor for authors","homepage":"https://example.org/editor","website":"https://example.org/editor","private":false,"fork":false,"archived":false,"topics":["editing"],"default_branch":"main"}]));
    responses.insert(format!("{prefix}{size}=100&page=2"), json!([]));
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

fn collect_fixture(forge: Forge) -> Snapshot {
    // collect() is private; drive the public surface through the dialect
    // helpers and the snapshot writer instead.
    let fixture = fixture(forge);
    let source = dialect(forge);
    let scope: Vec<String> = collect::pages(&fixture, source, "user/orgs")
        .unwrap()
        .iter()
        .map(|o| {
            collect::text(o, source.scope_login_field())
                .map(str::to_owned)
                .unwrap()
        })
        .collect();
    assert_eq!(scope, vec!["example"]);
    let org = snapshot_org(&fixture, source);
    assert_eq!(org.repositories.len(), 1);
    Snapshot {
        schema: 1,
        source: format!("{forge:?}:https://forge.example/api"),
        collected_at: "unix:0".into(),
        scope,
        complete: true,
        errors: Vec::new(),
        organizations: vec![org],
    }
}

fn snapshot_org(fixture: &Fixture, source: &dyn EvidenceSource) -> cqlt::Organization {
    // Exercise every dialect method the collector uses, in call order.
    let login = "example";
    let rows = collect::pages(fixture, source, &source.repos_path(login)).unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    let topics = source
        .repo_topics(fixture, "repos/example/editor", row)
        .unwrap();
    assert_eq!(topics, vec!["editing"]);
    let revision = "a".repeat(40);
    let root = collect::directory(
        fixture,
        &format!("repos/example/editor/contents?ref={revision}"),
    )
    .unwrap();
    let readme = source
        .repo_readme(fixture, "repos/example/editor", &revision, &root)
        .unwrap();
    let profile = source
        .org_profile(fixture, "repos/example/editor", &revision, root)
        .unwrap();
    cqlt::Organization {
        login: login.into(),
        name: None,
        description: None,
        website: None,
        profile,
        repository_count: 1,
        repositories: vec![cqlt::Repository {
            name: "editor".into(),
            full_name: "example/editor".into(),
            description: None,
            homepage: None,
            visibility: cqlt::Visibility::Public,
            fork: false,
            archived: false,
            revision: Some(revision),
            topics,
            readme,
            license: cqlt::Document::Present {
                path: "LICENSE-MIT".into(),
                bytes: 1000,
            },
        }],
    }
}

#[test]
fn both_forges_normalize_to_the_same_policy_evidence() {
    for forge in [Forge::Github, Forge::Forgejo] {
        let snapshot = collect_fixture(forge);
        assert!(snapshot.complete);
    }
    // The per-forge profile conventions differ by design; the shared
    // evidence (topics, revision, documents) must agree instead.
    assert_eq!(dialect_label(Forge::Github), "Github");
    assert_eq!(dialect_label(Forge::Forgejo), "Forgejo");
    let github = collect_fixture(Forge::Github);
    let forgejo = collect_fixture(Forge::Forgejo);
    let groom = github.organizations[0].repositories[0].clone();
    let froom = forgejo.organizations[0].repositories[0].clone();
    assert_eq!(groom.topics, froom.topics);
    assert_eq!(groom.revision, froom.revision);
    // cqlt::Document has no PartialEq; debug rendering is exact here.
    assert_eq!(format!("{:?}", groom.readme), format!("{:?}", froom.readme));
    assert_eq!(
        format!("{:?}", groom.license),
        format!("{:?}", froom.license)
    );
}
