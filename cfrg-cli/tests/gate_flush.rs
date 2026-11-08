//! `cfrg gate` end to end against local bare repositories standing in for GitHub: the newest state is pushed once for N commits,
//! the build lane goes first and is counted as an Actions run, and the repository gap holds the next push back.
use std::{fs, path::Path, process::Command};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.org",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn gate(dir: &Path, args: &[&str], token: bool) -> serde_json::Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cfrg"));
    command
        .current_dir(dir)
        .args(["gate", "--config", "cfg.json", "--origin"])
        .arg(format!("file://{}", dir.join("dest").display()))
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    if token {
        command.env("CFRG_GITHUB_TOKEN", "dummy");
    }
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn the_gate_pushes_the_newest_state_once_and_holds_the_next_push() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("dest/o")).unwrap();
    git(root, &["init", "-q", "--bare", "src.git"]);
    git(root, &["init", "-q", "work"]);
    let work = root.join("work");
    for i in 1..=3 {
        git(
            &work,
            &["commit", "-q", "--allow-empty", "-m", &format!("c{i}")],
        );
    }
    git(&work, &["push", "-q", "../src.git", "HEAD:refs/heads/main"]);
    for repo in ["bulk", "build"] {
        git(
            root,
            &["init", "-q", "--bare", &format!("dest/o/{repo}.git")],
        );
    }
    let source = root.join("src.git");
    fs::write(
        root.join("cfg.json"),
        serde_json::json!({
            "repos": {
                "o/bulk": {"lane": "bulk", "source": source},
                "o/build": {"lane": "build", "source": source}
            },
            "state_dir": root.join("state"),
            "paused_dir": root.join("paused"),
            "kill_switch": root.join("kill"),
            "quiet_fill": false
        })
        .to_string(),
    )
    .unwrap();

    gate(root, &["enqueue", "o/bulk", "o/build", "o/bulk"], false);
    let dry = gate(root, &["flush"], false);
    let verdicts: Vec<_> = dry["flush"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["repo"].as_str().unwrap(), r["verdict"].as_str().unwrap()))
        .collect();
    assert_eq!(
        verdicts,
        [("o/build", "would push"), ("o/bulk", "would push")]
    );
    assert!(
        git(root, &["-C", "dest/o/bulk.git", "for-each-ref"]).is_empty(),
        "a dry run pushes nothing"
    );

    let done = gate(root, &["flush", "--execute"], true);
    assert_eq!(done["flush"][0]["verdict"], "pushed");
    assert_eq!(
        git(root, &["-C", "dest/o/build.git", "rev-parse", "main"]),
        git(root, &["-C", "src.git", "rev-parse", "main"])
    );
    let status = gate(root, &["status"], false);
    assert_eq!(status["pushes"]["hour"], 2);
    assert_eq!(
        status["actions_runs"]["day"], 1,
        "only the build lane starts Actions runs"
    );

    git(&work, &["commit", "-q", "--allow-empty", "-m", "c4"]);
    git(&work, &["push", "-q", "../src.git", "HEAD:refs/heads/main"]);
    gate(root, &["enqueue", "o/bulk"], false);
    let held = gate(root, &["flush", "--execute"], true);
    assert!(held["flush"][0]["verdict"]
        .as_str()
        .unwrap()
        .starts_with("held: repository gap"));

    fs::create_dir_all(root.join("kill")).unwrap();
    let killed = gate(root, &["flush", "--execute"], true);
    assert!(killed["flush"][0]["verdict"]
        .as_str()
        .unwrap()
        .contains("kill switch"));
}
