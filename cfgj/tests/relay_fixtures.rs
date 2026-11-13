//! Recorded-fixture tests for the relay driver. Fixtures follow Forgejo's
//! documented response shapes; no live write was made.

use std::collections::BTreeMap;

use cfgj::relay::{Error, ForgejoRelay, Repo, Token};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "fgj_FIXTURETOKEN000000";

fn fx(name: &str) -> serde_json::Value {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn ok(code: u16, f: &str) -> ResponseTemplate {
    ResponseTemplate::new(code).set_body_json(fx(f))
}

async fn server() -> (MockServer, ForgejoRelay, Repo) {
    let s = MockServer::start().await;
    let r = ForgejoRelay::new(&s.uri(), &Token::new(TOKEN)).unwrap();
    (s, r, Repo::parse("corbet-cid/demo").unwrap())
}

#[tokio::test]
async fn issue_create_sends_the_token_and_body() {
    let (s, r, repo) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/corbet-cid/demo/issues"))
        .and(header("authorization", format!("token {TOKEN}").as_str()))
        .and(body_partial_json(json!({"title":"Notice","body":"text"})))
        .respond_with(ok(201, "issue_create.json"))
        .expect(1)
        .mount(&s)
        .await;
    let c = r.issue_create(&repo, "Notice", "text").await.unwrap();
    assert_eq!(c.number, 5);
    assert!(c.html_url.ends_with("/issues/5"));
}

#[tokio::test]
async fn notice_is_an_issue_plus_the_contribution_label() {
    let (s, r, repo) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/corbet-cid/demo/issues"))
        .respond_with(ok(201, "issue_create.json"))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/corbet-cid/demo/issues/5/labels"))
        .and(body_partial_json(json!({"labels":["contribution"]})))
        .respond_with(ok(200, "labels_add.json"))
        .expect(1)
        .mount(&s)
        .await;
    assert_eq!(
        r.notice_create(&repo, "Notice", "text")
            .await
            .unwrap()
            .number,
        5
    );
}

#[tokio::test]
async fn comment_posts_the_body() {
    let (s, r, repo) = server().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/corbet-cid/demo/issues/5/comments"))
        .and(body_partial_json(json!({"body":"Credited note"})))
        .respond_with(ok(201, "comment_create.json"))
        .expect(1)
        .mount(&s)
        .await;
    assert_eq!(r.comment(&repo, 5, "Credited note").await.unwrap(), 99);
}

#[tokio::test]
async fn issue_close_patches_the_state() {
    let (s, r, repo) = server().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/corbet-cid/demo/issues/5"))
        .and(body_partial_json(json!({"state":"closed"})))
        .respond_with(ok(201, "issue_closed.json"))
        .expect(1)
        .mount(&s)
        .await;
    r.issue_close(&repo, 5).await.unwrap();
}

#[tokio::test]
async fn workflow_dispatch_posts_ref_and_inputs() {
    let (s, r, repo) = server().await;
    Mock::given(method("POST"))
        .and(path(
            "/api/v1/repos/corbet-cid/demo/actions/workflows/relay.yml/dispatches",
        ))
        .and(body_partial_json(
            json!({"ref":"main","inputs":{"origin":"gl-12"}}),
        ))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&s)
        .await;
    let mut inputs = BTreeMap::new();
    inputs.insert("origin".to_string(), "gl-12".to_string());
    r.workflow_dispatch(&repo, "relay.yml", "main", &inputs)
        .await
        .unwrap();
}

#[tokio::test]
async fn arguments_are_validated_before_any_request() {
    let (s, r, repo) = server().await;
    for bad in ["", "x", "a/b/c", "a/..", "a b/c"] {
        assert!(Repo::parse(bad).is_err(), "{bad}");
    }
    let e = r
        .workflow_dispatch(&repo, "../x.yml", "main", &BTreeMap::new())
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Invalid(_)));
    assert!(s.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_token_is_never_printed() {
    let (s, r, repo) = server().await;
    assert!(!format!("{:?} {:?}", Token::new(TOKEN), r).contains(TOKEN));
    Mock::given(method("POST"))
        .and(path("/api/v1/repos/corbet-cid/demo/issues"))
        .respond_with(ok(401, "error_401.json"))
        .mount(&s)
        .await;
    let e = r.issue_create(&repo, "t", "b").await.unwrap_err();
    let shown = format!("{e} {e:?}");
    assert!(matches!(e, Error::Api(_)) && !shown.contains(TOKEN));
}
