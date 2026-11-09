//! `cfrg gate create` without network: dry runs, lane and config validation, the kill switch, paused
//! organisations, the creation budgets and the creation gap.
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

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

fn ledger(root: &Path, entries: &[(&str, &str, u64)]) {
    fs::create_dir_all(root.join("state")).unwrap();
    let lines: Vec<String> = entries
        .iter()
        .map(|(org, repo, ts)| {
            json!({"ts": ts, "kind": "create", "org": org, "repo": repo, "lane": "bulk"})
                .to_string()
        })
        .collect();
    fs::write(root.join("state/ledger.jsonl"), lines.join("\n") + "\n").unwrap();
}

#[test]
fn a_dry_run_prints_the_creation_and_needs_no_credential() {
    let (_dir, root) = setup();
    let out = success(
        &root,
        &["create", "o/bulk", "--private", "--description", "a mirror"],
    );
    assert_eq!(out["verdict"], "would create");
    assert_eq!(out["private"], true);
}

#[test]
fn an_unlisted_repository_is_refused() {
    let (_dir, root) = setup();
    assert!(refused(&root, &["create", "o/missing"]).contains("not listed"));
}

#[test]
fn a_long_or_control_description_is_refused() {
    let (_dir, root) = setup();
    let long = "x".repeat(351);
    assert!(refused(&root, &["create", "o/bulk", "--description", &long]).contains("350"));
    assert!(refused(&root, &["create", "o/bulk", "--description", "a\tb"]).contains("control"));
}

#[test]
fn the_kill_switch_and_a_paused_org_stop_a_creation() {
    let (_dir, root) = setup();
    fs::write(root.join("kill"), "").unwrap();
    assert!(refused(&root, &["create", "o/bulk"]).contains("kill switch"));
    fs::remove_file(root.join("kill")).unwrap();
    fs::create_dir_all(root.join("paused/o")).unwrap();
    assert!(refused(&root, &["create", "o/bulk"]).contains("paused"));
}

#[test]
fn the_daily_creation_budgets_apply() {
    let (_dir, root) = setup();
    let old = now() - 3600;
    ledger(&root, &[("o", "a", old), ("o", "b", old), ("p", "c", old)]);
    // Two creations in this organisation are below the per-organisation default of 3, but the gap is free.
    assert_eq!(
        success(&root, &["create", "o/bulk"])["verdict"],
        "would create"
    );
    ledger(&root, &[("o", "a", old), ("o", "b", old), ("o", "c", old)]);
    assert!(refused(&root, &["create", "o/bulk"]).contains("organisation's daily budget"));
    ledger(
        &root,
        &[
            ("a", "a", old),
            ("b", "a", old),
            ("c", "a", old),
            ("d", "a", old),
            ("e", "a", old),
            ("f", "a", old),
        ],
    );
    assert!(refused(&root, &["create", "o/bulk"]).contains("overall daily budget"));
}

#[test]
fn two_creations_keep_their_gap() {
    let (_dir, root) = setup();
    ledger(&root, &[("o", "a", now() - 100)]);
    assert!(refused(&root, &["create", "o/bulk"]).contains("creation gap"));
    ledger(&root, &[("o", "a", now() - 400)]);
    assert_eq!(
        success(&root, &["create", "o/bulk"])["verdict"],
        "would create"
    );
}
