//! Recorded-fixture tests for the relay driver. Fixtures follow GitLab's
//! documented response shapes; no live write was made.

use cglb::relay::{Error, GitlabRelay, Path, Token};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "glpat-FIXTURETOKEN000000";

fn fx(name: &str) -> serde_json::Value {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

async fn server() -> (MockServer, GitlabRelay, Path) {
    let s = MockServer::start().await;
    // The client probes the authenticated user when it is built.
    Mock::given(method("GET"))
        .and(path("/api/v4/user"))
        .and(header("PRIVATE-TOKEN", TOKEN))
        .respond_with(ok(200, "user.json"))
        .mount(&s)
        .await;
    let host = s.uri().trim_start_matches("http://").to_string();
    let r = GitlabRelay::new(&host, &Token::new(TOKEN), true)
        .await
        .unwrap();
    (s, r, Path::parse("grp/demo").unwrap())
}

fn ok(code: u16, f: &str) -> ResponseTemplate {
    ResponseTemplate::new(code).set_body_json(fx(f))
}

#[tokio::test]
async fn issue_create_sends_labels_and_the_token_header() {
    let (s, r, p) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/grp%2Fdemo/issues"))
        .and(header("PRIVATE-TOKEN", TOKEN))
        .and(body_string_contains("title=Found+a+bug"))
        .and(body_string_contains("labels=contribution"))
        .respond_with(ok(201, "issue_create.json"))
        .expect(1)
        .mount(&s)
        .await;
    let c = r
        .issue_create(&p, "Found a bug", "text", &["contribution".into()])
        .await
        .unwrap();
    assert_eq!(c.iid, 6);
}

#[tokio::test]
async fn issue_note_and_close() {
    let (s, r, p) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/grp%2Fdemo/issues/6/notes"))
        .and(body_string_contains("body=Credited+note"))
        .respond_with(ok(201, "note_create.json"))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/grp%2Fdemo/issues/6"))
        .and(body_string_contains("state_event=close"))
        .respond_with(ok(200, "issue_closed.json"))
        .expect(1)
        .mount(&s)
        .await;
    assert_eq!(r.issue_note(&p, 6, "Credited note").await.unwrap().id, 302);
    r.issue_close(&p, 6).await.unwrap();
}

#[tokio::test]
async fn label_create_posts_name_and_colour() {
    let (s, r, p) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/grp%2Fdemo/labels"))
        .and(body_string_contains("name=contribution"))
        .and(body_string_contains("color=%23f29513"))
        .respond_with(ok(201, "label_create.json"))
        .expect(1)
        .mount(&s)
        .await;
    r.label_create(&p, "contribution", "#f29513").await.unwrap();
}

#[tokio::test]
async fn merge_request_open_note_close() {
    let (s, r, p) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/grp%2Fdemo/merge_requests"))
        .and(body_string_contains("source_branch=cforge%2Fgh-5"))
        .and(body_string_contains("target_branch=main"))
        .respond_with(ok(201, "mr_create.json"))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/grp%2Fdemo/merge_requests/3/notes"))
        .respond_with(ok(201, "note_create.json"))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/grp%2Fdemo/merge_requests/3"))
        .and(body_string_contains("state_event=close"))
        .respond_with(ok(200, "mr_closed.json"))
        .expect(1)
        .mount(&s)
        .await;
    let mr = r
        .merge_request_open(&p, "cforge/gh-5", "main", "T", "D")
        .await
        .unwrap();
    assert_eq!(mr.iid, 3);
    r.merge_request_note(&p, 3, "n").await.unwrap();
    r.merge_request_close(&p, 3).await.unwrap();
}

#[tokio::test]
async fn subgroup_is_created_under_the_parent() {
    let (s, r, _) = server().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/groups/grp"))
        .respond_with(ok(200, "group_get.json"))
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v4/groups"))
        .and(body_string_contains("path=cforge"))
        .and(body_string_contains("parent_id=12"))
        .respond_with(ok(201, "subgroup_create.json"))
        .expect(1)
        .mount(&s)
        .await;
    let g = r
        .subgroup_create(&Path::parse("grp").unwrap())
        .await
        .unwrap();
    assert_eq!(g.full_path, "grp/cforge");
}

#[tokio::test]
async fn fork_goes_into_the_cforge_subgroup() {
    let (s, r, p) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/grp%2Fdemo/fork"))
        .and(body_string_contains("namespace_path=grp%2Fcforge"))
        .respond_with(ok(201, "fork_create.json"))
        .expect(1)
        .mount(&s)
        .await;
    let f = r
        .fork_into_relay_subgroup(&p, &Path::parse("grp").unwrap())
        .await
        .unwrap();
    assert_eq!(f.path_with_namespace, "grp/cforge/demo");
}

#[tokio::test]
async fn paths_are_validated() {
    for bad in ["", "a//b", "a/../b", "a b", "a/b?x", "/a"] {
        assert!(Path::parse(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn the_token_is_never_printed() {
    let (s, r, p) = server().await;
    assert!(!format!("{:?} {:?}", Token::new(TOKEN), r).contains(TOKEN));
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/grp%2Fdemo/issues"))
        .respond_with(ok(401, "error_401.json"))
        .mount(&s)
        .await;
    let e = r.issue_create(&p, "t", "d", &[]).await.unwrap_err();
    assert!(matches!(e, Error::Api(_)));
    let shown = format!("{e} {e:?}");
    assert!(shown.contains("401") && !shown.contains(TOKEN));
}
