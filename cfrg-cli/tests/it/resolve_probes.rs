//! Filesystem probe traps and credential-helper protocol, through the real
//! `cfrg` executable. Scratch fixtures only, no mounts, no network.
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

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

fn fixture_repo(root: &Path, name: &str) -> (PathBuf, String, String) {
    let work = root.join(format!("work-{name}"));
    fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-qb", "main"]);
    fs::write(work.join("value"), name).unwrap();
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
            name,
        ],
    );
    let sha = git(&work, &["rev-parse", "HEAD"]);
    let blob = git(&work, &["rev-parse", "HEAD:value"]);
    let bare = root.join(format!("{name}.git"));
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
    (bare, sha, blob)
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

fn fs_request(store: &str, path: &str, need: Value) -> Value {
    json!({
        "schema": 1,
        "canonical_base": "https://pointer.example",
        "repositories": [{"id": "a", "path": path, "ref": need,
                          "primary": "forgejo",
                          "source_urls": [format!("https://pointer.example/{path}")] }],
        "stores": [{"kind": "filesystem", "location": store,
                    "identity": "forgejo", "scope": ["acme"],
                    "trusted_single_user": true}],
        "timeout_secs": 20
    })
}

fn pin(sha: &str) -> Value {
    json!({"pinned": sha})
}

fn copy_dir_recursive(source: &Path, dest: &Path) {
    fs::create_dir_all(dest).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = dest.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir_recursive(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn fs_traps_symlink_alternates_and_config_include_fall_back() {
    let root = tempfile::tempdir().unwrap();
    let scope = root.path().join("scope");
    fs::create_dir_all(scope.join("acme")).unwrap();
    let (good, sha, blob) = fixture_repo(&root.path().join("build"), "good");
    let dest = scope.join("acme/good.git");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::rename(&good, &dest).unwrap();
    let store = scope.display().to_string();

    // Baseline: real commit probes Hit.
    let response = resolve(&fs_request(&store, "acme/good", pin(&sha)));
    assert_eq!(response["decisions"][0]["outcome"], "routed");

    // Missing commit is a miss, not an error.
    let response = resolve(&fs_request(
        &store,
        "acme/good",
        json!({"pinned": "0".repeat(40)}),
    ));
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");

    // A blob hash exists as an object but is not a commit: must miss.
    let response = resolve(&fs_request(&store, "acme/good", json!({"pinned": blob})));
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");

    // Symlink farm rejects to pointer even though the target holds the commit.
    symlink(&dest, scope.join("acme/link.git")).unwrap();
    let response = resolve(&fs_request(&store, "acme/link", pin(&sha)));
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");

    // Alternates file (promisor/lazy marker) rejects to pointer.
    fs::create_dir_all(scope.join("acme/alt.git/objects/info")).unwrap();
    fs::write(
        scope.join("acme/alt.git/objects/info/alternates"),
        "/elsewhere\n",
    )
    .unwrap();
    fs::write(scope.join("acme/alt.git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let response = resolve(&fs_request(&store, "acme/alt", pin(&sha)));
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");

    // Config include directive rejects to pointer.
    let inc = scope.join("acme/inc.git");
    fs::create_dir_all(inc.join("objects")).unwrap();
    fs::write(inc.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(inc.join("config"), "[include]\n\tpath = /tmp/evil\n").unwrap();
    let response = resolve(&fs_request(&store, "acme/inc", pin(&sha)));
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");

    // Unreadable config (directory in place of the file) fails closed.
    let unread = scope.join("acme/unread.git");
    fs::create_dir_all(unread.join("objects")).unwrap();
    fs::write(unread.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::create_dir(unread.join("config")).unwrap();
    let response = resolve(&fs_request(&store, "acme/unread", pin(&sha)));
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");

    // Symlinked HEAD fails closed even with intact objects.
    let symhead = scope.join("acme/symhead.git");
    fs::create_dir_all(symhead.join("objects")).unwrap();
    fs::write(
        symhead.join("config"),
        "[core]\n\trepositoryformatversion = 0\n",
    )
    .unwrap();
    symlink("/tmp/nonexistent-head-target", symhead.join("HEAD")).unwrap();
    let response = resolve(&fs_request(&store, "acme/symhead", pin(&sha)));
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");

    // Bare repo WITHOUT the `.git` suffix is never probed: the contract
    // covers exactly `{root}/{path}.git`, the same form the renderer emits.
    let plain = scope.join("acme/plain");
    copy_dir_recursive(&dest, &plain);
    let response = resolve(&fs_request(&store, "acme/plain", pin(&sha)));
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");

    // Moving ref verifies against the real branch; unknown ref falls back.
    let response = resolve(&fs_request(
        &store,
        "acme/good",
        json!({"moving": "refs/heads/main"}),
    ));
    assert_eq!(response["decisions"][0]["outcome"], "routed");
    let response = resolve(&fs_request(
        &store,
        "acme/good",
        json!({"moving": "refs/heads/nope"}),
    ));
    assert_eq!(response["decisions"][0]["outcome"], "canonical-pointer");
}

fn helper(args: &[&str], env_token: Option<(&str, &str)>, stdin: &str) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cfrg"));
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    if let Some((name, value)) = env_token {
        command.env(name, value);
    }
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn credential_helper_serves_get_only_for_exact_origin() {
    let base = [
        "credential-helper",
        "--env",
        "CFRG_RESOLVER_TEST_TOKEN",
        "--expect-origin",
        "https://forge.example:3001",
        "--username",
        "ci",
    ];
    // Happy path: exact origin answers username + password, nothing else.
    let mut args: Vec<&str> = base.to_vec();
    args.push("get");
    let output = helper(
        &args,
        Some(("CFRG_RESOLVER_TEST_TOKEN", "test-token-value")),
        "protocol=https\nhost=forge.example:3001\n\n",
    );
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("username=ci\n"));
    assert!(stdout.contains("password=test-token-value\n"));
    // Default-port spelling of the same authority also matches.
    let output = helper(
        &[
            "credential-helper",
            "--env",
            "CFRG_RESOLVER_TEST_TOKEN",
            "--expect-origin",
            "https://forge.example",
            "--username",
            "ci",
            "get",
        ],
        Some(("CFRG_RESOLVER_TEST_TOKEN", "test-token-value")),
        "protocol=https\nhost=forge.example\n\n",
    );
    assert!(output.status.success());
    // Foreign origin refused; token never surfaces.
    let output = helper(
        &args,
        Some(("CFRG_RESOLVER_TEST_TOKEN", "test-token-value")),
        "protocol=https\nhost=evil.example\n\n",
    );
    assert!(!output.status.success());
    let combined =
        String::from_utf8(output.stdout).unwrap() + &String::from_utf8(output.stderr).unwrap();
    assert!(!combined.contains("test-token-value"));
    // store/erase are silent no-ops.
    for action in ["store", "erase"] {
        let mut args: Vec<&str> = base.to_vec();
        args.push(action);
        let output = helper(
            &args,
            Some(("CFRG_RESOLVER_TEST_TOKEN", "test-token-value")),
            "protocol=https\nhost=forge.example:3001\n\n",
        );
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().is_empty());
    }
    // Missing credential fails without printing anything sensitive.
    let output = helper(&args, None, "protocol=https\nhost=forge.example:3001\n\n");
    assert!(!output.status.success());
}
