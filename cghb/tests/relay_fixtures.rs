//! Recorded-fixture tests for the relay driver. Fixtures are GitHub's documented
//! response shapes (no live write was made); the key is a throwaway generated
//! for these tests and protects nothing.

use cghb::relay::{AppCredentials, Error, GithubRelay, Redacted, Repo};
use serde_json::json;
use wiremock::matchers::{body_json, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = include_str!("fixtures/test-app-key.pem");
const TOKEN: &str = "ghs_FIXTURETOKEN0000000000000000000000";

fn fx(name: &str) -> serde_json::Value {
    let p = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn creds() -> AppCredentials {
    AppCredentials {
        app_id: 99,
        installation_id: 7,
        private_key_pem: Redacted::new(KEY),
    }
}

async fn server() -> (MockServer, GithubRelay, Repo) {
    let s = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/7/access_tokens"))
        // The App JWT authenticates the token request.
        .and(header_regex(
            "authorization",
            r"^Bearer [\w-]+\.[\w-]+\.[\w-]+$",
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(fx("access_token.json")))
        .mount(&s)
        .await;
    let relay = GithubRelay::new(&creds(), &s.uri()).unwrap();
    (s, relay, Repo::parse("corbet-labs/demo").unwrap())
}

fn bearer() -> wiremock::matchers::HeaderRegexMatcher {
    header_regex("authorization", &format!("^(Bearer|token) {TOKEN}$"))
}

#[tokio::test]
async fn branch_head_reads_the_sha() {
    let (s, r, repo) = server().await;
    Mock::given(method("GET"))
        .and(path("/repos/corbet-labs/demo/git/ref/heads/main"))
        .and(bearer())
        .respond_with(ResponseTemplate::new(200).set_body_json(fx("ref_get.json")))
        .mount(&s)
        .await;
    let sha = r.branch_head(&repo, "main").await.unwrap();
    assert_eq!(sha, "aa218f56b14c9653891f9e74264a383fa43fefbd");
}

#[tokio::test]
async fn branch_create_posts_the_ref() {
    let (s, r, repo) = server().await;
    Mock::given(method("POST"))
        .and(path("/repos/corbet-labs/demo/git/refs"))
        .and(bearer())
        .and(body_json(json!({"ref":"refs/heads/cforge/gl-12","sha":"aa218f56b14c9653891f9e74264a383fa43fefbd"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(fx("ref_create.json")))
        .expect(1)
        .mount(&s)
        .await;
    r.branch_create(
        &repo,
        "cforge/gl-12",
        "aa218f56b14c9653891f9e74264a383fa43fefbd",
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn branch_create_refuses_a_foreign_branch_before_any_request() {
    let (s, r, repo) = server().await;
    let e = r.branch_create(&repo, "main", "aa").await.unwrap_err();
    assert!(matches!(e, Error::Invalid(_)));
    assert!(r
        .branch_create(&repo, "cforge/../main", "aa")
        .await
        .is_err());
    // Nothing but possibly a token mint reached the server.
    for req in s.received_requests().await.unwrap() {
        assert!(req.url.path().ends_with("/access_tokens"));
    }
}

#[tokio::test]
async fn issue_create_with_label() {
    let (s, r, repo) = server().await;
    Mock::given(method("POST"))
        .and(path("/repos/corbet-labs/demo/issues"))
        .and(bearer())
        .and(body_json(
            json!({"title":"Found a bug","body":"text","labels":["contribution"]}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(fx("issue_create.json")))
        .expect(1)
        .mount(&s)
        .await;
    let c = r
        .issue_create(&repo, "Found a bug", "text", &["contribution".to_string()])
        .await
        .unwrap();
    assert_eq!(c.number, 42);
    assert_eq!(c.html_url, "https://github.com/corbet-labs/demo/issues/42");
}

#[tokio::test]
async fn comment_posts_the_body() {
    let (s, r, repo) = server().await;
    Mock::given(method("POST"))
        .and(path("/repos/corbet-labs/demo/issues/42/comments"))
        .and(body_json(json!({"body":"Credited note"})))
        .respond_with(ResponseTemplate::new(201).set_body_json(fx("comment_create.json")))
        .expect(1)
        .mount(&s)
        .await;
    assert_eq!(
        r.comment(&repo, 42, "Credited note").await.unwrap().id,
        1337
    );
}

#[tokio::test]
async fn labels_add_posts_the_names() {
    let (s, r, repo) = server().await;
    Mock::given(method("POST"))
        .and(path("/repos/corbet-labs/demo/issues/42/labels"))
        .and(body_json(json!({"labels":["contribution"]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fx("labels_add.json")))
        .expect(1)
        .mount(&s)
        .await;
    r.labels_add(&repo, 42, &["contribution".to_string()])
        .await
        .unwrap();
}

#[tokio::test]
async fn issue_close_patches_the_state() {
    let (s, r, repo) = server().await;
    Mock::given(method("PATCH"))
        .and(path("/repos/corbet-labs/demo/issues/42"))
        .and(body_json(json!({"state":"closed"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fx("state_closed.json")))
        .expect(1)
        .mount(&s)
        .await;
    r.issue_close(&repo, 42).await.unwrap();
}

#[tokio::test]
async fn pull_open_and_close() {
    let (s, r, repo) = server().await;
    Mock::given(method("POST"))
        .and(path("/repos/corbet-labs/demo/pulls"))
        .and(body_json(
            json!({"title":"T","head":"cforge/gl-12","base":"main","body":"B"}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(fx("pull_create.json")))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/corbet-labs/demo/pulls/7"))
        .and(body_json(json!({"state":"closed"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fx("state_closed.json")))
        .expect(1)
        .mount(&s)
        .await;
    let p = r
        .pull_open(&repo, "T", "cforge/gl-12", "main", "B")
        .await
        .unwrap();
    assert_eq!(p.number, 7);
    r.pull_close(&repo, 7).await.unwrap();
}

#[tokio::test]
async fn repository_names_are_validated() {
    for bad in ["", "x", "a/b/c", "a/..", "../b", "a b/c", "a/b?x=1"] {
        assert!(Repo::parse(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn tokens_and_keys_are_never_printed() {
    let (s, r, repo) = server().await;
    // Debug of every credential-holding type.
    let dbg = format!("{:?} {:?}", creds(), r);
    assert!(!dbg.contains("BEGIN RSA") && !dbg.contains(KEY.lines().nth(1).unwrap()));
    // An API failure carries the provider's message, never a credential.
    Mock::given(method("POST"))
        .and(path("/repos/corbet-labs/demo/issues"))
        .respond_with(ResponseTemplate::new(401).set_body_json(fx("error_401.json")))
        .mount(&s)
        .await;
    let e = r.issue_create(&repo, "t", "b", &[]).await.unwrap_err();
    let shown = format!("{e} {e:?}");
    assert!(shown.contains("Bad credentials"));
    assert!(!shown.contains(TOKEN) && !shown.contains("BEGIN RSA") && !shown.contains("eyJ"));
}
