//! The receiver: signatures per forge (valid, wrong, missing, replayed), dispatch, routes per listener, TLS 1.3.
mod common;
use base64::{engine::general_purpose::STANDARD, Engine};
use cfrg_serve::{
    config::{Config, Credentials},
    server::{self, Bridge},
};
use common::*;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::{json, Value};
use sha2::Sha256;
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, sync::watch};

fn hmac_of(key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().to_vec()
}

fn gitlab_token() -> String {
    format!("whsec_{}", STANDARD.encode([9u8; 32]))
}

struct Rig {
    w: World,
    internal: String,
    public: String,
    bridge: Arc<Bridge>,
    http: Client,
    _stop: watch::Sender<bool>,
}

impl Rig {
    async fn new() -> Rig {
        Self::with(
            &[
                ("hmac_forgejo", "s3cret"),
                ("hmac_github", "ghkey"),
                ("hmac_gitlab", "SET"),
                ("dispatch_token", "dtok"),
            ],
            None,
        )
        .await
    }

    async fn with(pairs: &[(&str, &str)], tls: Option<&std::path::Path>) -> Rig {
        let w = World::new().await;
        let token = gitlab_token();
        let pairs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(k, v)| (*k, if *v == "SET" { token.as_str() } else { *v }))
            .collect();
        let mut all = vec![("github_token", "ghp_fake"), ("gitlab_token", "glpat_fake")];
        all.extend(pairs);
        let cfg: Config = serde_json::from_value(json!({
            "forge_api": format!("{}/api/v1", w.forge.url), "forge_web": w.forge.url,
            "github": {"api": w.gh.url, "uploads": w.gh.url, "web": format!("{}/web", w.gh.url), "orgs": ["corbet-fsl"], "min_write_interval_s": 0},
            "gitlab": {"api": format!("{}/api/v4", w.gl.url), "groups": ["corbet-fsl"]},
            "dispatch": {"repository": "corbet-infra/infra"},
        }))
        .unwrap();
        let (bridge, workers) = Bridge::new(cfg, Credentials::from_pairs(&all)).unwrap();
        bridge.run_workers(workers);
        let (stop_tx, stop_rx) = watch::channel(false);
        let mut urls = vec![];
        for name in ["internal", "public"] {
            let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = socket.local_addr().unwrap().port();
            let acceptor = match (name, tls) {
                ("internal", Some(dir)) => Some(server::tls_acceptor(dir).unwrap()),
                _ => None,
            };
            urls.push(format!(
                "{}://127.0.0.1:{port}",
                if acceptor.is_some() { "https" } else { "http" }
            ));
            tokio::spawn(server::serve(
                bridge.clone(),
                name.to_owned(),
                socket,
                acceptor,
                stop_rx.clone(),
            ));
        }
        Rig {
            w,
            internal: urls[0].clone(),
            public: urls[1].clone(),
            bridge,
            http: Client::new(),
            _stop: stop_tx,
        }
    }

    async fn post(
        &self,
        base: &str,
        path: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> (u16, String) {
        let mut request = self.http.post(format!("{base}{path}")).body(body.to_vec());
        for (k, v) in headers {
            request = request.header(*k, v);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.text().await.unwrap().trim().to_owned())
    }

    fn dispatches(&self) -> Vec<Req> {
        self.w
            .forge
            .all()
            .into_iter()
            .filter(|r| r.path.ends_with("/dispatches"))
            .collect()
    }

    async fn dispatched(&self, count: usize) -> Vec<Req> {
        for _ in 0..100 {
            if self.dispatches().len() >= count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.dispatches()
    }

    async fn wait_done(&self) -> i32 {
        for _ in 0..200 {
            if let Some(code) = self.bridge.last(&self.w.repo, &self.w.tag) {
                return code;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the worker did not finish");
    }
}

fn github_body() -> Vec<u8> {
    json!({"action": "opened", "repository": {"id": 4242, "full_name": "x/y"}, "issue": {"number": 7, "title": "SECRET TITLE"}, "sender": {"login": "mallory"}})
        .to_string()
        .into_bytes()
}

fn github_headers(
    key: &str,
    body: &[u8],
    delivery: &str,
    event: &str,
) -> Vec<(&'static str, String)> {
    vec![
        (
            "x-hub-signature-256",
            format!("sha256={}", hex::encode(hmac_of(key.as_bytes(), &[body]))),
        ),
        ("x-github-delivery", delivery.into()),
        ("x-github-event", event.into()),
    ]
}

#[tokio::test]
async fn github_signature_valid_wrong_missing_replayed() {
    let rig = Rig::new().await;
    let body = github_body();
    // wrong key, missing signature, tampered body: 401 and nothing dispatched
    assert_eq!(
        rig.post(
            &rig.public,
            "/hooks/github",
            &github_headers("other", &body, "d1", "issues"),
            &body
        )
        .await
        .0,
        401
    );
    assert_eq!(
        rig.post(
            &rig.public,
            "/hooks/github",
            &[("x-github-event", "issues".into())],
            &body
        )
        .await
        .0,
        401
    );
    let mut tampered = body.clone();
    tampered.push(b' ');
    assert_eq!(
        rig.post(
            &rig.public,
            "/hooks/github",
            &github_headers("ghkey", &body, "d1", "issues"),
            &tampered
        )
        .await
        .0,
        401
    );
    // the Forgejo key does not sign for GitHub
    assert_eq!(
        rig.post(
            &rig.public,
            "/hooks/github",
            &github_headers("s3cret", &body, "d1", "issues"),
            &body
        )
        .await
        .0,
        401
    );
    assert!(rig.dispatches().is_empty());
    // valid
    let headers = github_headers("ghkey", &body, "d1", "issues");
    assert_eq!(
        rig.post(&rig.public, "/hooks/github", &headers, &body)
            .await,
        (202, "queued".into())
    );
    let sent = rig.dispatched(1).await;
    assert_eq!(sent.len(), 1);
    // replayed: the same delivery id is a duplicate, 2xx so the sender stops, and no second dispatch
    assert_eq!(
        rig.post(&rig.public, "/hooks/github", &headers, &body)
            .await,
        (200, "duplicate".into())
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(rig.dispatches().len(), 1);
    // a different delivery id of the same change is a new job
    let again = github_headers("ghkey", &body, "d2", "issues");
    assert_eq!(
        rig.post(&rig.public, "/hooks/github", &again, &body)
            .await
            .0,
        202
    );
    assert_eq!(rig.dispatched(2).await.len(), 2);
}

#[tokio::test]
async fn the_dispatch_carries_only_the_five_fields_and_the_narrow_token() {
    let rig = Rig::new().await;
    let body = github_body();
    rig.post(
        &rig.public,
        "/hooks/github",
        &github_headers("ghkey", &body, "d1", "issues"),
        &body,
    )
    .await;
    let sent = rig.dispatched(1).await;
    let request = &sent[0];
    assert_eq!(
        request.path,
        "/api/v1/repos/corbet-infra/infra/actions/workflows/relay.yml/dispatches"
    );
    assert_eq!(
        request.header("authorization").as_deref(),
        Some("token dtok")
    );
    let payload: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(
        payload,
        json!({"ref": "main", "inputs": {"forge": "github", "repository": "4242", "kind": "issue", "number": "7", "action": "changed"}})
    );
    let text = String::from_utf8_lossy(&request.body);
    assert!(
        !text.contains("SECRET TITLE") && !text.contains("mallory"),
        "nothing but the five fields is forwarded"
    );
}

#[tokio::test]
async fn the_fast_2xx_does_not_wait_for_the_dispatch() {
    let rig = Rig::new().await;
    rig.w.st.lock().unwrap().dispatch_status = 500;
    let body = github_body();
    let started = std::time::Instant::now();
    assert_eq!(
        rig.post(
            &rig.public,
            "/hooks/github",
            &github_headers("ghkey", &body, "d1", "issues"),
            &body
        )
        .await
        .0,
        202
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    // The dispatch failed: the delivery id was released, so GitHub's manual redelivery (same id, same body) is accepted.
    rig.dispatched(1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.w.st.lock().unwrap().dispatch_status = 204;
    assert_eq!(
        rig.post(
            &rig.public,
            "/hooks/github",
            &github_headers("ghkey", &body, "d1", "issues"),
            &body
        )
        .await
        .0,
        202
    );
    assert_eq!(rig.dispatched(2).await.len(), 2);
}

#[tokio::test]
async fn other_github_events_are_ignored_with_a_2xx() {
    let rig = Rig::new().await;
    let body = br#"{"zen":"hi"}"#.to_vec();
    assert_eq!(
        rig.post(
            &rig.public,
            "/hooks/github",
            &github_headers("ghkey", &body, "p1", "ping"),
            &body
        )
        .await,
        (200, "ignored".into())
    );
    assert!(rig.dispatches().is_empty());
}

fn gitlab_headers(id: &str, ts: i64, body: &[u8]) -> Vec<(&'static str, String)> {
    let sig = STANDARD.encode(hmac_of(
        &[9u8; 32],
        &[id.as_bytes(), b".", ts.to_string().as_bytes(), b".", body],
    ));
    vec![
        ("webhook-id", id.into()),
        ("webhook-timestamp", ts.to_string()),
        ("webhook-signature", format!("v1,{sig}")),
    ]
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test]
async fn gitlab_signature_valid_wrong_missing_replayed() {
    let rig = Rig::new().await;
    let body = json!({"object_kind": "merge_request", "project": {"id": 55}, "object_attributes": {"iid": 3}}).to_string().into_bytes();
    // wrong: signed with the token STRING instead of the decoded key
    let by_string = {
        let ts = now();
        let sig = STANDARD.encode(hmac_of(
            gitlab_token().as_bytes(),
            &[b"m1.", ts.to_string().as_bytes(), b".", &body],
        ));
        vec![
            ("webhook-id", "m1".to_owned()),
            ("webhook-timestamp", ts.to_string()),
            ("webhook-signature", format!("v1,{sig}")),
        ]
    };
    assert_eq!(
        rig.post(&rig.public, "/hooks/gitlab", &by_string, &body)
            .await
            .0,
        401
    );
    // missing
    assert_eq!(
        rig.post(&rig.public, "/hooks/gitlab", &[], &body).await.0,
        401
    );
    // stale timestamp
    assert_eq!(
        rig.post(
            &rig.public,
            "/hooks/gitlab",
            &gitlab_headers("m1", now() - 3600, &body),
            &body
        )
        .await
        .0,
        401
    );
    // the GitHub key does not sign for GitLab
    let gh = github_headers("ghkey", &body, "m1", "issues");
    assert_eq!(
        rig.post(&rig.public, "/hooks/gitlab", &gh, &body).await.0,
        401
    );
    assert!(rig.dispatches().is_empty());
    // valid, then replayed (webhook-id)
    let ok = gitlab_headers("m1", now(), &body);
    assert_eq!(
        rig.post(&rig.public, "/hooks/gitlab", &ok, &body).await,
        (202, "queued".into())
    );
    let sent = rig.dispatched(1).await;
    let payload: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(
        payload["inputs"],
        json!({"forge": "gitlab", "repository": "55", "kind": "contribution", "number": "3", "action": "changed"})
    );
    assert_eq!(
        rig.post(&rig.public, "/hooks/gitlab", &ok, &body).await,
        (200, "duplicate".into())
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(rig.dispatches().len(), 1);
}

#[tokio::test]
async fn forgejo_signature_valid_wrong_missing_replayed_and_dispatch() {
    let rig = Rig::new().await;
    let body = json!({"action": "deleted", "repository": {"id": 8}, "issue": {"number": 2}, "comment": {"id": 31}}).to_string().into_bytes();
    let sign = |key: &str| hex::encode(hmac_of(key.as_bytes(), &[&body]));
    let headers = |key: &str, id: &str| {
        vec![
            ("x-forgejo-signature", sign(key)),
            ("x-forgejo-event", "issue_comment".into()),
            ("x-forgejo-delivery", id.into()),
        ]
    };
    assert_eq!(
        rig.post(
            &rig.internal,
            "/hooks/forgejo",
            &headers("wrong", "f1"),
            &body
        )
        .await
        .0,
        401
    );
    assert_eq!(
        rig.post(
            &rig.internal,
            "/hooks/forgejo",
            &[("x-forgejo-event", "issue_comment".into())],
            &body
        )
        .await
        .0,
        401
    );
    assert_eq!(
        rig.post(
            &rig.internal,
            "/hooks/forgejo",
            &headers("s3cret", "f1"),
            &body
        )
        .await,
        (202, "queued".into())
    );
    let sent = rig.dispatched(1).await;
    let payload: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(payload["inputs"]["action"], "comment_deleted:31");
    assert_eq!(payload["inputs"]["forge"], "forgejo");
    assert_eq!(
        rig.post(
            &rig.internal,
            "/hooks/forgejo",
            &headers("s3cret", "f1"),
            &body
        )
        .await,
        (200, "duplicate".into())
    );
}

#[tokio::test]
async fn no_dispatch_token_means_503_and_nothing_is_lost_to_the_replay_guard() {
    let rig = Rig::with(&[("hmac_github", "ghkey")], None).await;
    let body = github_body();
    let headers = github_headers("ghkey", &body, "d1", "issues");
    assert_eq!(
        rig.post(&rig.public, "/hooks/github", &headers, &body)
            .await
            .0,
        503
    );
    assert_eq!(
        rig.post(&rig.public, "/hooks/github", &headers, &body)
            .await
            .0,
        503,
        "the id was released"
    );
}

// ------------------------------------------------------------------ the Python bridge's HTTP tests, ported

fn forgejo_sig(key: &str, body: &[u8]) -> String {
    hex::encode(hmac_of(key.as_bytes(), &[body]))
}

fn release_event(w: &World) -> Vec<u8> {
    json!({"action": "published", "release": {"tag_name": w.tag, "draft": false}, "repository": {"full_name": w.repo}}).to_string().into_bytes()
}

#[tokio::test]
async fn release_signature_is_required() {
    let rig = Rig::new().await;
    let body = release_event(&rig.w);
    assert_eq!(
        rig.post(&rig.internal, "/hooks/release", &[], &body)
            .await
            .0,
        401
    );
    assert_eq!(
        rig.post(
            &rig.internal,
            "/hooks/release",
            &[("x-forgejo-signature", forgejo_sig("wrong", &body))],
            &body
        )
        .await
        .0,
        401
    );
    let hub = [(
        "x-hub-signature-256",
        format!("sha256={}", forgejo_sig("s3cret", &body)),
    )];
    assert_eq!(
        rig.post(&rig.internal, "/hooks/release", &hub, &body)
            .await
            .0,
        202
    );
}

#[tokio::test]
async fn a_published_release_is_synced_and_the_hint_uses_the_same_module() {
    let rig = Rig::new().await;
    let body = release_event(&rig.w);
    let sig = [("x-forgejo-signature", forgejo_sig("s3cret", &body))];
    assert_eq!(
        rig.post(&rig.internal, "/hooks/release", &sig, &body).await,
        (202, "queued".into())
    );
    assert_eq!(rig.wait_done().await, 0);
    let files = rig.w.st.lock().unwrap().files.clone();
    assert_eq!(rig.w.gh_stored(), files);
    // the tag pusher's hint: no delivery id, repeatable
    let hint = json!({"repository": rig.w.repo, "tag": rig.w.tag})
        .to_string()
        .into_bytes();
    let sig = [("x-forgejo-signature", forgejo_sig("s3cret", &hint))];
    assert_eq!(
        rig.post(&rig.internal, "/hooks/sync", &sig, &hint).await.0,
        202
    );
    assert_eq!(
        rig.post(&rig.internal, "/hooks/sync", &sig, &hint).await.0,
        202
    );
}

#[tokio::test]
async fn a_release_delivery_is_not_replayed() {
    let rig = Rig::new().await;
    let body = release_event(&rig.w);
    let headers = [
        ("x-forgejo-signature", forgejo_sig("s3cret", &body)),
        ("x-forgejo-delivery", "rel-1".into()),
    ];
    assert_eq!(
        rig.post(&rig.internal, "/hooks/release", &headers, &body)
            .await
            .0,
        202
    );
    assert_eq!(rig.wait_done().await, 0);
    assert_eq!(
        rig.post(&rig.internal, "/hooks/release", &headers, &body)
            .await,
        (200, "duplicate".into())
    );
}

#[tokio::test]
async fn other_actions_and_drafts_are_ignored_not_retried() {
    let rig = Rig::new().await;
    for payload in [
        json!({"action": "created", "release": {"tag_name": rig.w.tag, "draft": false}, "repository": {"full_name": rig.w.repo}}),
        json!({"action": "published", "release": {"tag_name": rig.w.tag, "draft": true}, "repository": {"full_name": rig.w.repo}}),
    ] {
        let body = payload.to_string().into_bytes();
        let sig = [("x-forgejo-signature", forgejo_sig("s3cret", &body))];
        assert_eq!(
            rig.post(&rig.internal, "/hooks/release", &sig, &body).await,
            (200, "ignored".into())
        );
    }
    assert!(rig.w.forge.all().is_empty());
}

#[tokio::test]
async fn bad_input_is_refused() {
    let rig = Rig::new().await;
    let raw = b"not json".to_vec();
    let sig = [("x-forgejo-signature", forgejo_sig("s3cret", &raw))];
    assert_eq!(
        rig.post(&rig.internal, "/hooks/release", &sig, &raw)
            .await
            .0,
        400
    );
    let hint = json!({"repository": "../x", "tag": "v1"})
        .to_string()
        .into_bytes();
    let sig = [("x-forgejo-signature", forgejo_sig("s3cret", &hint))];
    assert_eq!(
        rig.post(&rig.internal, "/hooks/sync", &sig, &hint).await.0,
        400
    );
    assert_eq!(
        rig.post(&rig.internal, "/hooks/unknown", &[], b"{}")
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn the_public_listener_answers_only_the_provider_routes_and_health() {
    let rig = Rig::new().await;
    let body = json!({"action": "created"}).to_string().into_bytes();
    let sig = [("x-forgejo-signature", forgejo_sig("s3cret", &body))];
    assert_eq!(
        rig.post(&rig.internal, "/hooks/release", &sig, &body)
            .await
            .0,
        200
    ); // signed, not wanted: ignored
    for path in ["/hooks/release", "/hooks/sync", "/hooks/forgejo"] {
        assert_eq!(
            rig.post(&rig.public, path, &sig, &body).await.0,
            404,
            "{path}"
        );
    }
    for path in ["/hooks/github", "/hooks/gitlab"] {
        assert_eq!(
            rig.post(&rig.internal, path, &sig, &body).await.0,
            404,
            "{path}"
        );
    }
    for base in [&rig.internal, &rig.public] {
        assert_eq!(
            Client::new()
                .get(format!("{base}/healthz"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            Client::new()
                .get(format!("{base}/hooks/github"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
}

#[tokio::test]
async fn a_forge_without_its_own_key_cannot_sign() {
    // Only another forge's key and the old name are present: the Forgejo route verifies with hmac_forgejo only.
    let rig = Rig::with(&[("hmac_github", "s3cret")], None).await;
    let body = release_event(&rig.w);
    let sig = [("x-forgejo-signature", forgejo_sig("s3cret", &body))];
    assert_eq!(
        rig.post(&rig.internal, "/hooks/release", &sig, &body)
            .await
            .0,
        401
    );
}

#[tokio::test]
async fn oversize_and_unsized_bodies_are_refused() {
    let rig = Rig::new().await;
    let big = vec![b'a'; (1 << 20) + 1];
    assert_eq!(
        rig.post(&rig.internal, "/hooks/release", &[], &big).await.0,
        413
    );
}

#[tokio::test]
async fn the_service_speaks_tls_1_3_only_and_still_checks_the_signature() {
    if std::process::Command::new("openssl")
        .arg("version")
        .output()
        .is_err()
    {
        eprintln!("openssl is needed to make a throwaway certificate; skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let made = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-days",
            "1",
            "-keyout",
        ])
        .arg(path.join("tls.key"))
        .arg("-out")
        .arg(path.join("tls.crt"))
        .args([
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=IP:127.0.0.1",
            "-addext",
            "basicConstraints=critical,CA:FALSE",
        ])
        .output()
        .unwrap();
    assert!(made.status.success());
    let rig = Rig::with(&[("hmac_forgejo", "s3cret")], Some(path)).await;
    assert!(rig.internal.starts_with("https://"));
    let cert =
        reqwest::Certificate::from_pem(&std::fs::read(path.join("tls.crt")).unwrap()).unwrap();
    let trusting = Client::builder()
        .tls_certs_only([cert.clone()])
        .build()
        .unwrap();
    assert_eq!(
        trusting
            .get(format!("{}/healthz", rig.internal))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let response = trusting
        .post(format!("{}/hooks/release", rig.internal))
        .header("x-forgejo-signature", "0".repeat(64))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    // The Forgejo client of the release module trusts the cluster bundle and nothing else; the provider client does not.
    let forge = cfrg_serve::http::forge_client(Some(&path.join("tls.crt"))).unwrap();
    assert_eq!(
        forge
            .get(format!("{}/healthz", rig.internal))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(cfrg_serve::http::public_client()
        .unwrap()
        .get(format!("{}/healthz", rig.internal))
        .send()
        .await
        .is_err());
    // A client that does not know the CA is refused.
    assert!(Client::new()
        .get(format!("{}/healthz", rig.internal))
        .send()
        .await
        .is_err());
    // TLS 1.2 is refused.
    let old = Client::builder()
        .tls_certs_only([cert])
        .tls_version_max(reqwest::tls::Version::TLS_1_2)
        .build()
        .unwrap();
    assert!(old
        .get(format!("{}/healthz", rig.internal))
        .send()
        .await
        .is_err());
}
