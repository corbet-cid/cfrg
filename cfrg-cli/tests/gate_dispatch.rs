//! `cfrg gate dispatch` without network: dry runs, lane and config validation, the kill switch and paused
//! organisations, and the workflow/ref/input limits.
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn setup() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::write(
        root.join("cfg.json"),
        json!({
            "repos": {
                "o/build": {"lane": "build", "source": root.join("source")},
                "o/bulk": {"lane": "bulk", "source": root.join("source")}
            },
            "state_dir": root.join("state"),
            "paused_dir": root.join("paused"),
            "kill_switch": root.join("kill")
        })
        .to_string(),
    )
    .unwrap();
    (dir, root)
}

fn gate(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cfrg"))
        .current_dir(root)
        .args(["gate", "--config", "cfg.json"])
        .args(args)
        .env_remove("CFRG_GITHUB_TOKEN")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap()
}

fn success(root: &Path, args: &[&str]) -> Value {
    let out = gate(root, args);
    assert!(
        out.status.success(),
        "cfrg gate {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn refused(root: &Path, args: &[&str]) -> String {
    let out = gate(root, args);
    assert!(!out.status.success(), "expected a refusal for {args:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn refused_owned(root: &Path, args: &[String]) -> String {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    refused(root, &refs)
}

#[test]
fn the_gate_build_lane_dry_run_would_dispatch_and_writes_no_ledger() {
    let (_tmp, root) = setup();
    let report = success(
        &root,
        &[
            "dispatch",
            "o/build",
            "ccid-remote.yml",
            "main",
            "--input",
            "job=macos",
            "--input",
            "sha=0123456789abcdef0123456789abcdef01234567",
            "--input",
            "request=ab12cd",
        ],
    );
    assert_eq!(report["repo"], "o/build");
    assert_eq!(report["verdict"], "would dispatch");
    assert_eq!(report["workflow"], "ccid-remote.yml");
    assert_eq!(report["ref"], "main");
    assert!(
        !root.join("state/ledger.jsonl").exists(),
        "a dry run writes no ledger entry"
    );
}

#[test]
fn the_gate_allows_only_listed_build_lane_repositories_to_dispatch() {
    let (_tmp, root) = setup();
    let error = refused(&root, &["dispatch", "o/bulk", "ci.yml", "main"]);
    assert!(error.contains("build-lane"), "{error}");
    let error = refused(&root, &["dispatch", "o/missing", "ci.yml", "main"]);
    assert!(error.contains("not listed"), "{error}");
    assert!(!root.join("state/ledger.jsonl").exists());
}

#[test]
fn the_gate_kill_switch_refuses_before_any_credential_or_call() {
    let (_tmp, root) = setup();
    fs::create_dir_all(root.join("kill")).unwrap();
    let out = gate(
        &root,
        &["dispatch", "o/build", "ci.yml", "main", "--execute"],
    );
    assert!(!out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["verdict"], "kill switch is set");
    assert!(String::from_utf8_lossy(&out.stderr).contains("kill switch"));
    assert!(
        !root.join("state/ledger.jsonl").exists(),
        "a refused dispatch writes no ledger entry"
    );
}

#[test]
fn the_gate_refuses_a_paused_organisation() {
    let (_tmp, root) = setup();
    fs::create_dir_all(root.join("paused/o")).unwrap();
    let out = gate(
        &root,
        &["dispatch", "o/build", "ci.yml", "main", "--execute"],
    );
    assert!(!out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["verdict"], "organisation is paused");
    assert!(!root.join("state/ledger.jsonl").exists());
}

#[test]
fn the_gate_refuses_invalid_workflows_refs_and_inputs() {
    let (_tmp, root) = setup();
    for workflow in ["dir/ci.yml", "ci.txt", "ci.yml.bak", " ci.yml", "ci.yml "] {
        let error = refused(&root, &["dispatch", "o/build", workflow, "main"]);
        assert!(error.contains("workflow"), "workflow {workflow:?}: {error}");
    }
    for reference in ["feature/x", ".main", "ma in", ""] {
        let error = refused(&root, &["dispatch", "o/build", "ci.yml", reference]);
        assert!(error.contains("ref"), "ref {reference:?}: {error}");
    }
    // A leading dash needs `--` to reach our validator instead of clap's option parser.
    let error = refused(&root, &["dispatch", "o/build", "ci.yml", "--", "-main"]);
    assert!(error.contains("ref"), "ref \"-main\": {error}");

    let too_many: Vec<String> = (0..11).map(|i| format!("k{i}=v")).collect();
    let long = format!("a={}", "x".repeat(201));
    let cases = [
        vec!["bad".to_string()],
        vec!["a b=1".to_string()],
        vec!["a=x\ny".to_string()],
        vec!["a=1".to_string(), "a=2".to_string()],
        too_many,
        vec![long],
    ];
    for inputs in cases {
        let mut args: Vec<String> = ["dispatch", "o/build", "ci.yml", "main"]
            .into_iter()
            .map(String::from)
            .collect();
        for input in &inputs {
            args.push("--input".into());
            args.push(input.clone());
        }
        let error = refused_owned(&root, &args);
        assert!(
            error.to_ascii_lowercase().contains("input"),
            "inputs {inputs:?}: {error}"
        );
    }
    assert!(!root.join("state/ledger.jsonl").exists());
}
