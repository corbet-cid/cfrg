#![cfg(unix)]
#[path = "support/forge_sync.rs"]
mod support;
use std::{fs, os::unix::fs::PermissionsExt};
use support::{git, Fixture};

#[test]
fn plans_then_copies_all_heads_and_exact_tag_objects_without_touching_other_locations() {
    let f = Fixture::new();
    let first = f.commit("first");
    git(&f.source, &["branch", "feature"]);
    git(&f.source, &["tag", "-am", "release", "v1"]);
    let tag = git(&f.source, &["rev-parse", "refs/tags/v1"]);
    let (_, planned) = f.report(&["--all-refs", "--destination", "replica"]);
    assert_eq!(planned["replicas"][0]["state"], "planned");
    assert!(git(&f.replica, &["for-each-ref", "--format=%(refname)"]).is_empty());
    let (output, report) = f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(report["complete"], true);
    assert_eq!(report["repository_complete"], false); // The declared offline location was not selected.
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/feature"]), first);
    assert_eq!(git(&f.replica, &["rev-parse", "refs/tags/v1"]), tag);
    let latest = f.commit("second");
    let (_, report) = f.report(&[
        "--ref",
        "refs/heads/main",
        "--destination",
        "replica",
        "--apply",
    ]);
    assert_eq!(report["complete"], true);
    assert_eq!(
        report["replicas"][0]["refs"][0]["reason"],
        "verified-after-push"
    );
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/main"]), latest);
    assert_eq!(git(&f.replica, &["rev-parse", "refs/tags/v1"]), tag);
}

#[test]
fn divergence_tag_replacement_and_replica_only_refs_block_all_writes_to_that_replica() {
    let f = Fixture::new();
    let first = f.commit("first");
    git(&f.source, &["tag", "-am", "release", "v1"]);
    let (_, report) = f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    assert_eq!(report["complete"], true);
    let second = f.commit("source change");
    git(&f.source, &["checkout", "-q", "--detach", &first]);
    let diverged = f.commit("independent change");
    git(
        &f.replica,
        &["fetch", "-q", f.source.to_str().unwrap(), &diverged],
    );
    git(&f.replica, &["update-ref", "refs/heads/main", &diverged]);
    git(&f.replica, &["update-ref", "refs/tags/v1", &first]); // Same peeled commit, different tag object.
    git(&f.replica, &["update-ref", "refs/heads/keep", &first]);
    git(&f.source, &["checkout", "-q", "main"]);
    git(&f.source, &["branch", "new-branch", &second]);
    let (output, report) = f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    assert!(!output.status.success());
    assert_eq!(report["replicas"][0]["state"], "blocked");
    let reasons = report["replicas"][0]["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["reason"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(reasons.contains(&"diverged-branch"));
    assert!(reasons.contains(&"tag-conflict"));
    assert_eq!(report["replicas"][0]["retained_refs"][0], "refs/heads/keep");
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/main"]), diverged);
    assert!(!std::process::Command::new("git")
        .current_dir(&f.replica)
        .args(["show-ref", "--verify", "refs/heads/new-branch"])
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn offline_replica_is_pending_while_other_replicas_can_catch_up_later() {
    let f = Fixture::new();
    let first = f.commit("first");
    let (output, report) = f.report(&[
        "--all-refs",
        "--destination",
        "replica",
        "--destination",
        "offline",
        "--apply",
    ]);
    assert!(!output.status.success());
    assert_eq!(report["complete"], false);
    assert_eq!(report["replicas"][0]["state"], "updated");
    assert_eq!(report["replicas"][1]["state"], "pending");
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/main"]), first);
    let restored = f.root.path().join("missing.git");
    fs::create_dir(&restored).unwrap();
    git(&restored, &["init", "--bare", "-q"]);
    let (output, report) = f.report(&[
        "--all-refs",
        "--destination",
        "replica",
        "--destination",
        "offline",
        "--apply",
    ]);
    assert!(output.status.success());
    assert_eq!(report["repository_complete"], true);
    assert_eq!(git(&restored, &["rev-parse", "refs/heads/main"]), first);
}

#[test]
fn ahead_replica_and_failed_atomic_push_are_never_reported_complete() {
    let f = Fixture::new();
    let first = f.commit("first");
    let second = f.commit("second");
    f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    git(&f.source, &["reset", "--hard", &first]);
    let (_, report) = f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    assert_eq!(report["replicas"][0]["refs"][0]["reason"], "replica-ahead");
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/main"]), second);
    git(&f.source, &["reset", "--hard", &second]);
    f.commit("third");
    git(&f.source, &["branch", "new-branch"]);
    let hook = f.replica.join("hooks/pre-receive");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let (output, report) = f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    assert!(!output.status.success());
    assert_eq!(report["replicas"][0]["state"], "pending");
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/main"]), second);
    assert!(!std::process::Command::new("git")
        .current_dir(&f.replica)
        .args(["show-ref", "--verify", "refs/heads/new-branch"])
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn historical_lfs_pointers_and_gitlinks_block_promotion_even_after_removal() {
    let f = Fixture::new();
    let first = f.commit("first");
    fs::write(
        f.source.join("large"),
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 12345\n",
            "a".repeat(64)
        ),
    )
    .unwrap();
    git(&f.source, &["add", "large"]);
    git(
        &f.source,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{first},dependency"),
        ],
    );
    git(&f.source, &["commit", "-qm", "external payloads"]);
    git(&f.source, &["rm", "--cached", "dependency", "large"]);
    git(&f.source, &["commit", "-qm", "remove external payloads"]);
    let (output, report) = f.report(&["--all-refs", "--destination", "replica", "--apply"]);
    assert!(!output.status.success());
    assert_eq!(report["source_reason"], "external-content-unverified");
    assert_eq!(report["content"]["lfs_required"], true);
    assert_eq!(report["content"]["submodules_required"], true);
    assert_eq!(report["complete"], false);
    assert!(git(&f.replica, &["for-each-ref", "--format=%(refname)"]).is_empty());
}

#[test]
fn arbitrary_refs_duplicate_destinations_and_missing_source_refs_are_refused() {
    let f = Fixture::new();
    f.commit("first");
    for args in [
        vec![
            "--ref",
            "refs/heads/*",
            "--destination",
            "replica",
            "--apply",
        ],
        vec!["--ref", "HEAD", "--destination", "replica", "--apply"],
        vec!["--all-refs", "--destination", "source", "--apply"],
        vec![
            "--all-refs",
            "--destination",
            "replica",
            "--destination",
            "replica",
            "--apply",
        ],
    ] {
        assert!(!f.run(&args).status.success());
    }
    let (_, report) = f.report(&[
        "--ref",
        "refs/heads/missing",
        "--destination",
        "replica",
        "--apply",
    ]);
    assert_eq!(report["source_reason"], "source-ref-missing");
    assert!(git(&f.replica, &["for-each-ref", "--format=%(refname)"]).is_empty());
}

#[test]
fn bounded_runner_preserves_binary_batch_input_and_null_stdin_default() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("objects");
    let bytes = b"\0\xff\n spaced \0\n";
    fs::write(&input, bytes).unwrap();
    let runner = cfrg::process::Runner::new(
        directory.path().into(),
        std::env::vars_os().collect(),
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(
        runner
            .run_bytes_with_input_file(&["cat".into()], &input)
            .unwrap(),
        bytes
    );
    assert_eq!(fs::read(&input).unwrap(), bytes);
    assert_eq!(runner.run(&["cat".into()], true).unwrap(), "");
}

#[test]
fn noncanonical_lfs_pointers_never_claim_payload_replication() {
    for (prefix, padding) in [
        ("\n\t ", 0),
        ("\u{a0}\u{2003}", 0),
        ("", 2000),
        ("", 17 * 1024 * 1024),
    ] {
        let f = Fixture::new();
        f.commit("first");
        fs::write(
            f.source.join("large"),
            format!(
                "{prefix}version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 12345\n{}",
                "a".repeat(64),
                " ".repeat(padding)
            ),
        )
        .unwrap();
        git(&f.source, &["add", "large"]);
        git(&f.source, &["commit", "-qm", "noncanonical pointer"]);
        let (output, report) = f.report(&["--all-refs", "--to", "replica", "--apply"]);
        assert!(!output.status.success());
        assert_eq!(report["content"]["lfs_required"], true);
        assert!(git(&f.replica, &["for-each-ref"]).is_empty());
    }
}

#[test]
fn remote_diagnostics_and_trace_configuration_cannot_leak_into_reports_or_logs() {
    let f = Fixture::new();
    f.commit("first");
    let hook = f.replica.join("hooks/pre-receive");
    fs::write(
        &hook,
        "#!/bin/sh\necho fixture-credential-secret >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    let trace = f.root.path().join("trace");
    let output = f
        .command(&["--all-refs", "--to", "replica", "--apply"])
        .env("GIT_TRACE", &trace)
        .env("GIT_TRACE_CURL", &trace)
        .env("GIT_TRACE_PACKET", &trace)
        .env("GIT_CURL_VERBOSE", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!trace.exists());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-credential-secret"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-credential-secret"));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["replicas"][0]["reason"], "push-not-confirmed");
}

#[test]
fn ambient_git_configuration_cannot_redirect_sync() {
    let f = Fixture::new();
    let first = f.commit("first");
    let config = f.root.path().join("ambient-config");
    fs::write(
        &config,
        "[url \"ext::false\"]\n insteadOf = https://source.example/team/source.git\n",
    )
    .unwrap();
    let output = f
        .command(&["--all-refs", "--to", "replica", "--apply"])
        .env("GIT_CONFIG_GLOBAL", config)
        .env(
            "GIT_CONFIG_PARAMETERS",
            "'url.ext::false.insteadOf=https://source.example/team/source.git'",
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(git(&f.replica, &["rev-parse", "refs/heads/main"]), first);
}

#[test]
fn excessive_ref_inventory_fails_closed_before_replica_writes() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let f = Fixture::new();
    let first = f.commit("first");
    let mut child = Command::new("git")
        .current_dir(&f.source)
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for index in 0..4096 {
        writeln!(input, "create refs/heads/branch-{index} {first}").unwrap();
    }
    drop(input);
    assert!(child.wait().unwrap().success());
    let (output, report) = f.report(&["--all-refs", "--to", "replica", "--apply"]);
    assert!(!output.status.success());
    assert_eq!(report["complete"], false);
    assert!(git(&f.replica, &["for-each-ref"]).is_empty());
}

#[test]
fn secondary_http_redirect_cannot_route_requests_to_a_different_location() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};
    let f = Fixture::new();
    let first = f.commit("first");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut requests = Vec::new();
        while Instant::now() < deadline {
            if let Ok((mut stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut buffer = [0; 4096];
                let size = stream.read(&mut buffer).unwrap();
                requests.push(String::from_utf8_lossy(&buffer[..size]).into_owned());
                write!(stream, "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/primary\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        requests
    });
    let output = f
        .command(&["--all-refs", "--to", "replica", "--apply"])
        .env(
            "GIT_CONFIG_KEY_1",
            format!("url.http://127.0.0.1:{port}/.insteadOf"),
        )
        .env("GIT_CONFIG_COUNT", "5")
        .env("GIT_CONFIG_KEY_4", "protocol.http.allow")
        .env("GIT_CONFIG_VALUE_4", "always")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(requests[0].starts_with("GET /replica.git/info/refs"));
    assert_eq!(git(&f.source, &["rev-parse", "refs/heads/main"]), first);
    assert!(git(&f.replica, &["for-each-ref"]).is_empty());
}

#[test]
fn policy_url_and_repository_path_injection_is_refused_before_transport() {
    let f = Fixture::new();
    let first = f.commit("first");
    let policy: serde_json::Value = serde_json::from_slice(&fs::read(&f.policy).unwrap()).unwrap();
    for path in [
        "team/../escape",
        "team/%2e%2e",
        "team/repo;touch",
        "team/$(touch)",
        "team/repo\n",
    ] {
        let mut hostile = policy.clone();
        hostile["repositories"]["widget"]["locations"]["replica"] = path.into();
        fs::write(&f.policy, serde_json::to_vec(&hostile).unwrap()).unwrap();
        assert!(!f
            .run(&["--all-refs", "--to", "replica", "--apply"])
            .status
            .success());
    }
    for url in [
        "https://secret@replica.example",
        "https://replica.example?target=source",
        "https://replica.example/%2e%2e",
        "ext::sh -c true",
    ] {
        let mut hostile = policy.clone();
        hostile["forges"]["replica"]["url"] = url.into();
        fs::write(&f.policy, serde_json::to_vec(&hostile).unwrap()).unwrap();
        assert!(!f
            .run(&["--all-refs", "--to", "replica", "--apply"])
            .status
            .success());
    }
    assert_eq!(git(&f.source, &["rev-parse", "refs/heads/main"]), first);
    assert!(git(&f.replica, &["for-each-ref"]).is_empty());
}

#[test]
fn large_ordinary_blobs_are_inspected_without_exceeding_capture_limits() {
    let f = Fixture::new();
    f.commit("first");
    fs::write(f.source.join("ordinary"), vec![b'x'; 17 * 1024 * 1024]).unwrap();
    git(&f.source, &["add", "ordinary"]);
    git(&f.source, &["commit", "-qm", "ordinary large blob"]);
    let (output, report) = f.report(&["--all-refs", "--to", "replica", "--apply"]);
    assert!(output.status.success(), "{report}");
    assert_eq!(report["complete"], true);
}
