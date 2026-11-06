//! `cfrg gate delete-repo` without network: the allowlist, the kill switch, a paused organisation, the pacing of
//! deletions and the refusal to act without a credential to verify with. The GitHub reads (private? head?) are unit
//! tests in `src/delete.rs`.
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn setup() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::write(
        root.join("cfg.json"),
        json!({
            "repos": {},
            "state_dir": root.join("state"),
            "paused_dir": root.join("paused"),
            "kill_switch": root.join("kill")
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        root.join("allow.json"),
        json!({"repos": {"old/thing": {"head": HEAD}}}).to_string(),
    )
    .unwrap();
    (dir, root)
}

fn delete(root: &Path, repository: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cfrg"))
        .current_dir(root)
        .args(["gate", "--config", "cfg.json", "delete-repo", repository])
        .args(["--allowlist", "allow.json"])
        .args(extra)
        .env_remove("CFRG_GITHUB_TOKEN")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap()
}

fn refused(root: &Path, repository: &str, extra: &[&str]) -> String {
    let out = delete(root, repository, extra);
    assert!(!out.status.success(), "expected a refusal for {repository}");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn a_repository_off_the_allowlist_is_refused_even_with_execute() {
    let (_dir, root) = setup();
    let text = refused(&root, "old/other", &["--execute"]);
    assert!(text.contains("not on the delete allowlist"), "{text}");
}

#[test]
fn nothing_is_verified_or_deleted_without_a_credential() {
    let (_dir, root) = setup();
    for extra in [&[][..], &["--execute"][..]] {
        let text = refused(&root, "old/thing", extra);
        assert!(text.contains("CFRG_GITHUB_TOKEN is not set"), "{text}");
    }
    assert!(!root.join("state/ledger.jsonl").exists());
}

#[test]
fn the_kill_switch_and_a_paused_organisation_stop_a_deletion() {
    let (_dir, root) = setup();
    fs::create_dir(root.join("paused")).unwrap();
    fs::create_dir(root.join("paused/old")).unwrap();
    assert!(refused(&root, "old/thing", &[]).contains("organisation is paused"));
    fs::remove_dir(root.join("paused/old")).unwrap();
    fs::create_dir(root.join("kill")).unwrap();
    assert!(refused(&root, "old/thing", &[]).contains("kill switch is set"));
}

#[test]
fn a_recent_deletion_of_any_repository_holds_the_next_one_back() {
    let (_dir, root) = setup();
    fs::create_dir(root.join("state")).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let entry =
        json!({"ts": now - 5, "kind": "delete", "org": "another", "repo": "repo", "lane": "bulk"});
    fs::write(root.join("state/ledger.jsonl"), format!("{entry}\n")).unwrap();
    let text = refused(&root, "old/thing", &[]);
    assert!(text.contains("delete pacing"), "{text}");
}

#[test]
fn an_invalid_allowlist_is_refused() {
    let (_dir, root) = setup();
    fs::write(
        root.join("allow.json"),
        json!({"repos": {"old/thing": {"head": "abc"}}}).to_string(),
    )
    .unwrap();
    assert!(refused(&root, "old/thing", &[]).contains("Invalid delete allowlist"));
}
