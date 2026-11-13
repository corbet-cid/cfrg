//! `cfrg relay` against a throwaway PostgreSQL. Ignored unless `CFRG_RELAY_TEST_PG` holds a libpq key/value
//! connection string, e.g. `host=/tmp/sock user=postgres dbname=postgres`.

use std::{fs, process::Command};

const CONFIG: &str = r#"{
  "relay": {"forgejo": ["100"], "github": ["200"]},
  "repos": [{"name": "acme/app", "members": {
    "forgejo": {"host": "forge.example", "id": "fj1", "path": "acme/app"},
    "github": {"host": "github.com", "id": "gh1", "path": "acme-gh/app"}}}]
}"#;

fn placement(contributed: &str) -> String {
    format!(
        r#"{{"native":{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"repositories":[],"contributed":[{contributed}]}}}}"#
    )
}

fn event(repo: &str) -> String {
    format!(
        r#"{{"forge":"github","repo":"{repo}","kind":"issue","number":1,"action":{{"type":"changed"}},"actor":{{"id":"u1","login":"alice"}}}}"#
    )
}

fn run(dir: &std::path::Path, placement: &str, args: &[&str]) -> (Option<i32>, String, String) {
    fs::write(dir.join("relay.json"), CONFIG).unwrap();
    fs::write(dir.join("placement.json"), placement).unwrap();
    let base = std::env::var("CFRG_RELAY_TEST_PG").expect("CFRG_RELAY_TEST_PG");
    let output = Command::new(env!("CARGO_BIN_EXE_cfrg"))
        .arg("relay")
        .arg("--config")
        .arg(dir.join("relay.json"))
        .arg("--placement")
        .arg(dir.join("placement.json"))
        .args(args)
        .env("CFRG_RELAY_DATABASE", base)
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
#[ignore = "needs CFRG_RELAY_TEST_PG"]
fn sweep_with_nothing_pending_exits_zero_and_a_contributed_repository_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (code, out, err) = run(dir.path(), &placement(""), &["sweep"]);
    assert_eq!(code, Some(0), "{err}");
    assert!(out.contains(r#""objects":0"#), "{out}");

    // GitHub copy declared as a contributed project: dropped before any read or write, and not an error.
    let contributed = r#"{"name":"app","upstream":"https://github.com/acme-gh/app"}"#;
    let (code, out, err) = run(
        dir.path(),
        &placement(contributed),
        &["apply", &event("gh1")],
    );
    assert_eq!(code, Some(0), "{err}");
    assert!(
        out.contains("contributed project") && out.contains("dropped"),
        "{out}"
    );

    // A listed repository with no live driver cannot be read now: non-zero, so the job is re-run.
    let (code, _, err) = run(dir.path(), &placement(""), &["apply", &event("gh1")]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("Nothing was done"), "{err}");

    // An unlisted repository is dropped quietly.
    let (code, out, _) = run(dir.path(), &placement(""), &["apply", &event("elsewhere")]);
    assert_eq!(code, Some(0));
    assert!(out.contains("not relayed"), "{out}");
}
