//! Primary-lookup enrichment through the real `cfrg` executable: bounded
//! unauthenticated HEAD against a loopback redirect authority, exact-match
//! mapping, fail-closed fallback. Scratch fixtures only, no mounts.
//!
//! The loopback server plays the pointer authority (never the store): it
//! answers HEAD with 302 Locations while moving-ref verification runs
//! against filesystem stores.
#![forbid(unsafe_code)]
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Scripted loopback pointer authority: answers `expect` connections then
/// stops (plus a 10s backstop). `respond` maps (port, raw request) to the
/// response body, or `None` to hold the connection open (timeout probe).
struct Pointer {
    port: u16,
    log: Arc<Mutex<Vec<String>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

fn serve<F>(expect: usize, respond: F) -> Pointer
where
    F: Fn(u16, &str) -> Option<String> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let records = Arc::clone(&log);
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut served = 0usize;
        while Instant::now() < deadline && served < expect {
            if let Ok((mut stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buffer = [0; 8192];
                let size = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..size]).into_owned();
                match respond(port, &request) {
                    Some(body) => {
                        let _ = write!(stream, "{body}");
                    }
                    None => {
                        // Hold open past any client timeout, then close.
                        std::thread::sleep(Duration::from_secs(8));
                    }
                }
                records.lock().unwrap().push(request);
                served += 1;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    Pointer {
        port,
        log,
        handle: Some(handle),
    }
}

impl Pointer {
    fn stop(mut self) -> Vec<String> {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.log.lock().unwrap().clone()
    }
}

fn git(workdir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(workdir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Filesystem store fixture: bare repo with `refs/heads/main`.
fn fs_store(root: &Path, path: &str) -> PathBuf {
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-qb", "main"]);
    fs::write(work.join("value"), path).unwrap();
    git(&work, &["add", "value"]);
    git(
        &work,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            path,
        ],
    );
    let store_root = root.join("stores");
    let bare = store_root.join(format!("{path}.git"));
    fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(
        &work,
        &[
            "clone",
            "--bare",
            "--",
            &work.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    store_root
}

fn moving_request(
    pointer_port: u16,
    identities: Value,
    path: &str,
    store_root: &Path,
    primary: Value,
) -> Value {
    json!({
        "schema": 1,
        "canonical_base": "https://pointer.example",
        "aliases": [],
        "repositories": [{
            "id": path, "path": path,
            "ref": {"moving": "refs/heads/main"},
            "primary": primary,
            "source_urls": [
                format!("https://pointer.example/{path}"),
                format!("https://pointer.example/{path}.git"),
            ],
        }],
        "stores": [{
            "kind": "filesystem",
            "location": store_root.display().to_string(),
            "identity": "forgejo",
            "scope": ["acme"],
            "trusted_single_user": true,
        }],
        "timeout_secs": 30,
        "primary_source": {
            "pointer_base": format!("http://127.0.0.1:{pointer_port}"),
            "identities": identities,
            "timeout_secs": 5,
        },
    })
}

fn resolve(request: &Value) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("request.json");
    fs::write(&path, serde_json::to_vec(request).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cfrg"))
        .args(["resolve", "--request"])
        .arg(&path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "resolve: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn forgejo_primary_routes_moving_ref() {
    let dir = tempfile::tempdir().unwrap();
    let store_root = fs_store(dir.path(), "acme/widget");
    let pointer = serve(1, |port, _| {
        Some(format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/acme/widget\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ))
    });
    let port = pointer.port;
    let request = moving_request(
        port,
        json!({format!("http://127.0.0.1:{port}"): "forgejo"}),
        "acme/widget",
        &store_root,
        Value::Null,
    );
    let response = resolve(&request);
    let decision = &response["decisions"][0];
    assert_eq!(decision["outcome"], "routed");
    assert_eq!(decision["primary"], "forgejo");
    let log = pointer.stop();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].starts_with("HEAD /acme/widget "), "{:?}", log[0]);
    assert!(
        !log[0].to_lowercase().contains("authorization"),
        "lookup must send zero auth: {:?}",
        log[0]
    );
}

#[test]
fn non_forgejo_location_stays_pointer_despite_secondary() {
    let dir = tempfile::tempdir().unwrap();
    let store_root = fs_store(dir.path(), "acme/widget");
    let pointer = serve(1, |_, _| {
        Some(
            "HTTP/1.1 302 Found\r\nLocation: http://hub.example/acme/widget\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        )
    });
    let port = pointer.port;
    // The forgejo secondary is present and holds the ref, but the live
    // primary decision names an unknown base: pointer, never Forgejo.
    let request = moving_request(
        port,
        json!({format!("http://127.0.0.1:{port}"): "forgejo"}),
        "acme/widget",
        &store_root,
        Value::Null,
    );
    let response = resolve(&request);
    let decision = &response["decisions"][0];
    assert_eq!(decision["outcome"], "canonical-pointer");
    assert!(decision["primary"].is_null());
    assert_eq!(pointer.stop().len(), 1);
}

#[test]
fn changed_primary_between_jobs_is_observed() {
    let dir = tempfile::tempdir().unwrap();
    let store_root = fs_store(dir.path(), "acme/widget");
    let mode = Arc::new(Mutex::new(false));
    let flag = Arc::clone(&mode);
    let pointer = serve(2, move |port, _| {
        let second = *flag.lock().unwrap();
        let base = if second {
            "http://hub.example".to_string()
        } else {
            format!("http://127.0.0.1:{port}")
        };
        Some(format!(
            "HTTP/1.1 302 Found\r\nLocation: {base}/acme/widget\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ))
    });
    let port = pointer.port;
    let build = || {
        moving_request(
            port,
            json!({format!("http://127.0.0.1:{port}"): "forgejo"}),
            "acme/widget",
            &store_root,
            Value::Null,
        )
    };
    let first = resolve(&build());
    assert_eq!(first["decisions"][0]["outcome"], "routed");
    *mode.lock().unwrap() = true;
    let second = resolve(&build());
    // No permanent cache: the changed decision is observed on the next job.
    assert_eq!(second["decisions"][0]["outcome"], "canonical-pointer");
    assert_eq!(pointer.stop().len(), 2);
}

#[test]
fn lookup_failures_fall_back_to_pointer() {
    let dir = tempfile::tempdir().unwrap();
    let store_root = fs_store(dir.path(), "acme/widget");
    // 403 with no Location.
    let denied = serve(1, |_, _| {
        Some("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string())
    });
    let port = denied.port;
    let request = moving_request(
        port,
        json!({format!("http://127.0.0.1:{port}"): "forgejo"}),
        "acme/widget",
        &store_root,
        Value::Null,
    );
    let response = resolve(&request);
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");
    denied.stop();
    // Bad Location (query trick): exact match fails.
    let tricky = serve(1, |port, _| {
        Some(format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/acme/widget?x=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ))
    });
    let port = tricky.port;
    let request = moving_request(
        port,
        json!({format!("http://127.0.0.1:{port}"): "forgejo"}),
        "acme/widget",
        &store_root,
        Value::Null,
    );
    let response = resolve(&request);
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");
    tricky.stop();
    // Timeout (held connection, 1s lookup bound).
    let slow = serve(1, |_, _| None);
    let port = slow.port;
    let mut request = moving_request(
        port,
        json!({format!("http://127.0.0.1:{port}"): "forgejo"}),
        "acme/widget",
        &store_root,
        Value::Null,
    );
    request["primary_source"]["timeout_secs"] = json!(1);
    let response = resolve(&request);
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");
    slow.stop();
    // Unreachable authority.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = closed.local_addr().unwrap().port();
    drop(closed);
    let request = moving_request(
        dead,
        json!({format!("http://127.0.0.1:{dead}"): "forgejo"}),
        "acme/widget",
        &store_root,
        Value::Null,
    );
    let response = resolve(&request);
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");
}

#[test]
fn pinned_repos_skip_lookup_and_policy_primary_wins() {
    let dir = tempfile::tempdir().unwrap();
    let _store_root = fs_store(dir.path(), "acme/widget");
    // Pinned repo, zero stores: pointer, and the authority sees NO lookup.
    // (The server thread is detached via drop; the log assertion right after
    // the synchronous resolve is deterministic.)
    let pointer = serve(1, |_, _| {
        Some("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/acme/widget\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string())
    });
    let port = pointer.port;
    let seen = Arc::clone(&pointer.log);
    let pinned = json!({
        "schema": 1,
        "canonical_base": "https://pointer.example",
        "aliases": [],
        "repositories": [{
            "id": "acme/widget", "path": "acme/widget",
            "ref": {"pinned": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
            "primary": Value::Null,
            "source_urls": ["https://pointer.example/acme/widget"],
        }],
        "stores": [],
        "timeout_secs": 30,
        "primary_source": {
            "pointer_base": format!("http://127.0.0.1:{port}"),
            "identities": {format!("http://127.0.0.1:{port}"): "forgejo"},
            "timeout_secs": 5,
        },
    });
    let response = resolve(&pinned);
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");
    assert!(seen.lock().unwrap().is_empty(), "pinned must skip lookup");
    // Server thread detached (no join: expect-count never reached); the
    // empty log above is the deterministic assertion.
    // Explicit policy primary wins over the feed: no lookup is attempted.
    let dir = tempfile::tempdir().unwrap();
    let store_root = fs_store(dir.path(), "acme/widget");
    let pointer = serve(1, |_, _| {
        Some("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/acme/widget\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string())
    });
    let port = pointer.port;
    let seen = Arc::clone(&pointer.log);
    let request = moving_request(
        port,
        json!({format!("http://127.0.0.1:{port}"): "forgejo"}),
        "acme/widget",
        &store_root,
        json!("forgejo"),
    );
    let response = resolve(&request);
    assert_eq!(response["decisions"][0]["outcome"], "routed");
    assert_eq!(response["decisions"][0]["primary"], "forgejo");
    assert!(
        seen.lock().unwrap().is_empty(),
        "policy primary skips lookup"
    );
}

/// Anonymous Forgejo API probes: a loopback API that answers public commit /
/// branch reads, denies private paths without auth, and records whether any
/// request carried an Authorization header (dummy tokens only, never printed).
fn api_request(
    store_port: u16,
    path: &str,
    ref_json: Value,
    primary: Value,
    credential_env: Option<Value>,
) -> Value {
    let mut store = json!({
        "kind": "http-forge",
        "provider": "forgejo",
        "location": format!("http://127.0.0.1:{store_port}"),
        "identity": "forgejo",
        "scope": ["acme"],
        "trusted_single_user": false,
    });
    if let Some(env) = credential_env {
        store["credential_env"] = env;
    }
    json!({
        "schema": 1,
        "canonical_base": "https://pointer.example",
        "aliases": [],
        "repositories": [{
            "id": path, "path": path,
            "ref": ref_json,
            "primary": primary,
            "source_urls": [format!("https://pointer.example/{path}")],
        }],
        "stores": [store],
        "timeout_secs": 30,
    })
}

fn resolve_with(request: &Value, envs: &[(&str, &str)], drops: &[&str]) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("request.json");
    fs::write(&path, serde_json::to_vec(request).unwrap()).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cfrg"));
    cmd.args(["resolve", "--request"])
        .arg(&path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    for k in drops {
        cmd.env_remove(k);
    }
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "resolve: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn anonymous_public_probes_hit_without_auth() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let records = Arc::clone(&seen);
    let server = serve(2, move |port, request| {
        let authed = request.to_lowercase().contains("authorization:");
        records.lock().unwrap().push(authed);
        let first = request.lines().next().unwrap_or_default().to_owned();
        let body = if first.contains("/git/commits/") {
            let sha = first
                .split("/git/commits/")
                .nth(1)
                .unwrap_or_default()
                .split_whitespace()
                .next()
                .unwrap_or_default();
            format!("{{\"sha\":\"{sha}\"}}")
        } else if first.contains("/branches/") {
            "{\"name\":\"main\"}".to_string()
        } else {
            return Some(
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
            );
        };
        Some(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body} port={port}",
            body.len()
        ))
    });
    let port = server.port;
    // Pinned hit, credential_env absent entirely.
    let sha = "f".repeat(40);
    let request = api_request(
        port,
        "acme/widget",
        json!({"pinned": sha}),
        Value::Null,
        None,
    );
    let response = resolve_with(&request, &[], &["CFRG_RESOLVER_TOKEN"]);
    assert_eq!(response["decisions"][0]["outcome"], "routed");
    // Moving hit, valid env name configured but UNSET in the job env.
    let request = api_request(
        port,
        "acme/gear",
        json!({"moving": "refs/heads/main"}),
        json!("forgejo"),
        Some(json!("CFRG_RESOLVER_UNSET_TOKEN")),
    );
    let response = resolve_with(&request, &[], &["CFRG_RESOLVER_UNSET_TOKEN"]);
    assert_eq!(response["decisions"][0]["outcome"], "routed");
    let hits = server.stop();
    assert_eq!(hits.len(), 2, "expected exactly the two API probes");
    let flags = seen.lock().unwrap().clone();
    assert_eq!(flags, vec![false, false], "no auth may be sent");
}

#[test]
fn anonymous_private_is_denied_and_malformed_token_fails_closed() {
    // Private path: 401 without auth.
    let server = serve(1, |_, request| {
        if request.to_lowercase().contains("authorization:") {
            return Some(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"name\":\"main\"}"
                    .to_string(),
            );
        }
        Some(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
        )
    });
    let port = server.port;
    let request = api_request(
        port,
        "acme/secret",
        json!({"moving": "refs/heads/main"}),
        json!("forgejo"),
        None,
    );
    let response = resolve_with(&request, &[], &[]);
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");
    server.stop();
    // Validly-named but malformed token value (space is non-graphic):
    // fail closed, never silently anonymous.
    let server = serve(1, |_, _| {
        Some(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"name\":\"main\"}"
                .to_string(),
        )
    });
    let port = server.port;
    let request = api_request(
        port,
        "acme/widget",
        json!({"moving": "refs/heads/main"}),
        json!("forgejo"),
        Some(json!("CFRG_RESOLVER_BAD_VALUE")),
    );
    let response = resolve_with(&request, &[("CFRG_RESOLVER_BAD_VALUE", "has space")], &[]);
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");
    server.stop();
    // Malformed credential_env NAME: whole request rejected at validation.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("request.json");
    let request = api_request(
        port,
        "acme/widget",
        json!({"moving": "refs/heads/main"}),
        json!("forgejo"),
        Some(json!("bad name!")),
    );
    fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cfrg"))
        .args(["resolve", "--request"])
        .arg(&path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(!output.status.success(), "malformed env name must fail");
}

#[test]
fn supplied_token_reaches_only_the_intended_store() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let records = Arc::clone(&seen);
    let server = serve(1, move |_, request| {
        records
            .lock()
            .unwrap()
            .push(request.to_lowercase().contains("authorization:"));
        Some(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"name\":\"main\"}"
                .to_string(),
        )
    });
    let port = server.port;
    let request = api_request(
        port,
        "acme/widget",
        json!({"moving": "refs/heads/main"}),
        json!("forgejo"),
        Some(json!("CFRG_RESOLVER_SCOPED_TOKEN")),
    );
    let response = resolve_with(
        &request,
        &[("CFRG_RESOLVER_SCOPED_TOKEN", "dummy-scoped-token")],
        &[],
    );
    assert_eq!(response["decisions"][0]["outcome"], "routed");
    let flags = server.stop();
    assert_eq!(flags.len(), 1);
    assert!(
        seen.lock().unwrap().clone() == vec![true],
        "configured store receives auth"
    );
}

#[cfg(unix)]
#[test]
fn non_unicode_token_fails_closed_without_probe() {
    use std::os::unix::ffi::OsStringExt;
    // Child-only non-UTF8 env via OsString (no unsafe parent mutation):
    // VarError::NotUnicode must fail closed, never downgrade to anonymous.
    let server = serve(1, |_, _| {
        Some(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"name\":\"main\"}"
                .to_string(),
        )
    });
    let port = server.port;
    let request = api_request(
        port,
        "acme/widget",
        json!({"moving": "refs/heads/main"}),
        json!("forgejo"),
        Some(json!("CFRG_RESOLVER_NONUTF8_FIXTURE")),
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("request.json");
    fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
    let raw = std::ffi::OsString::from_vec(vec![0xff, 0xfe, b'x']);
    let output = Command::new(env!("CARGO_BIN_EXE_cfrg"))
        .args(["resolve", "--request"])
        .arg(&path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("CFRG_RESOLVER_NONUTF8_FIXTURE", raw)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "resolve: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");
    let hits = server.stop();
    assert!(hits.is_empty(), "malformed credential must not probe");
}

#[test]
fn declared_placement_routes_primary_and_preserves_foreign_pins() {
    let dir = tempfile::tempdir().unwrap();
    let store = fs_store(dir.path(), "acme/widget");
    let placement = dir.path().join("placement.json");
    let mut request = moving_request(
        9,
        json!({"https://home.example": "forgejo", "https://github.com": "github"}),
        "acme/widget",
        &store,
        Value::Null,
    );
    request["primary_source"]["placement_file"] = json!(placement);
    for (primaries, expected) in [
        (json!({}), "routed"),
        (
            json!({"acme/widget":"https://github.com"}),
            "canonical-pointer",
        ),
        (
            json!({"acme/widget":"https://unknown.example"}),
            "canonical-pointer",
        ),
    ] {
        fs::write(
            &placement,
            serde_json::to_vec(&json!({
                "version":1,"default":"https://home.example","primaries":primaries,
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(resolve(&request)["decisions"][0]["outcome"], expected);
    }
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(store.join("acme/widget.git"))
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let hash = String::from_utf8(output.stdout).unwrap().trim().to_string();
    request["repositories"][0]["ref"] = json!({"pinned":hash});
    assert_eq!(resolve(&request)["decisions"][0]["outcome"], "routed");
    request["repositories"][0]["ref"] = json!({"moving":"refs/heads/main"});
    for invalid in [
        "not json",
        r#"{"version":2,"default":"https://home.example","primaries":{}}"#,
    ] {
        fs::write(&placement, invalid).unwrap();
        assert_eq!(
            resolve(&request)["decisions"][0]["outcome"],
            "canonical-pointer"
        );
    }
    fs::remove_file(&placement).unwrap();
    assert_eq!(
        resolve(&request)["decisions"][0]["outcome"],
        "canonical-pointer"
    );
}

/// Emergency fallback through the real executable: a dead canonical pointer
/// (a closed loopback port, refused at once) lets a declared store serve a
/// moving ref whose primary is another forge, with a warning; without the
/// opt-in nothing changes.
#[test]
fn dead_pointer_takes_the_emergency_fallback_only_when_declared() {
    let dir = tempfile::tempdir().unwrap();
    let store_root = fs_store(dir.path(), "acme/widget");
    let build = |emergency: bool| {
        let mut request = moving_request(1, json!({}), "acme/widget", &store_root, json!("github"));
        let object = request.as_object_mut().unwrap();
        object.remove("primary_source");
        object.insert("canonical_base".into(), json!("https://127.0.0.1:1"));
        object.insert("emergency_fallback".into(), json!(emergency));
        for url in request["repositories"][0]["source_urls"]
            .as_array_mut()
            .unwrap()
        {
            *url = json!(url
                .as_str()
                .unwrap()
                .replace("pointer.example", "127.0.0.1:1"));
        }
        request
    };
    let declared = resolve(&build(true));
    let decision = &declared["decisions"][0];
    assert_eq!(decision["outcome"], "emergency-fallback", "{decision}");
    assert!(decision["note"]
        .as_str()
        .unwrap()
        .starts_with("EMERGENCY FALLBACK"));
    assert!(decision["via"].as_str().unwrap().starts_with("file://"));
    let undeclared = resolve(&build(false));
    assert_eq!(undeclared["decisions"][0]["outcome"], "canonical-pointer");
}
