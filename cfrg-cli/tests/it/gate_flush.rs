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

#[test]
fn held_tags_are_neither_pushed_nor_compared_while_branches_keep_flowing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("dest/o")).unwrap();
    git(root, &["init", "-q", "--bare", "src.git"]);
    git(root, &["init", "-q", "work"]);
    let work = root.join("work");
    git(&work, &["commit", "-q", "--allow-empty", "-m", "c1"]);
    git(&work, &["push", "-q", "../src.git", "HEAD:refs/heads/main"]);
    git(root, &["init", "-q", "--bare", "dest/o/bulk.git"]);
    let source = root.join("src.git");
    let write_config = |hold_tags: bool| {
        fs::write(
            root.join("cfg.json"),
            serde_json::json!({
                "repos": {"o/bulk": {"lane": "bulk", "source": source}},
                "budgets": {"repo_min_gap_secs": 0},
                "state_dir": root.join("state"),
                "paused_dir": root.join("paused"),
                "kill_switch": root.join("kill"),
                "quiet_fill": false,
                "hold_tags": hold_tags
            })
            .to_string(),
        )
        .unwrap();
    };
    write_config(true);

    // A release tag arrives next to the branch: with the hold, the push carries the branch and leaves the tag behind.
    git(&work, &["tag", "v1"]);
    git(&work, &["push", "-q", "../src.git", "refs/tags/v1"]);
    gate(root, &["enqueue", "o/bulk"], false);
    let dry = gate(root, &["flush"], false);
    assert_eq!(dry["flush"][0]["verdict"], "would push");
    assert_eq!(
        dry["flush"][0]["refs_changed"], 1,
        "only the branch counts while tags are held"
    );
    assert_eq!(gate(root, &["status"], false)["hold_tags"], true);
    let done = gate(root, &["flush", "--execute"], true);
    assert_eq!(done["flush"][0]["verdict"], "pushed");
    assert_eq!(
        git(root, &["-C", "dest/o/bulk.git", "rev-parse", "main"]),
        git(root, &["-C", "src.git", "rev-parse", "main"])
    );
    assert!(
        git(root, &["-C", "dest/o/bulk.git", "tag", "--list"]).is_empty(),
        "no tag reaches the destination while tags are held"
    );

    // The tag alone does not make the repository differ: the branches are in sync.
    gate(root, &["enqueue", "o/bulk"], false);
    let again = gate(root, &["flush", "--execute"], true);
    assert_eq!(again["flush"][0]["verdict"], "in sync, dequeued");

    // Lifting the hold pushes the tag that was waiting (the declared hold is a switch, nothing is forgotten).
    write_config(false);
    gate(root, &["enqueue", "o/bulk"], false);
    let lifted = gate(root, &["flush", "--execute"], true);
    assert_eq!(lifted["flush"][0]["verdict"], "pushed");
    assert_eq!(
        git(root, &["-C", "dest/o/bulk.git", "tag", "--list"]).trim(),
        "v1"
    );
}

/// The forge router keeps relay pull request heads on the copy under `refs/heads/cforge/` (ccid wiki "Forge-router" §4): a flush
/// leaves them alone, does not count them (the repository is in sync afterwards, so it is not pushed again on every flush), never
/// pushes a primary branch of that namespace, and hints the forge bridge about each tag it brought to the copy (R45).
#[test]
fn a_relay_head_on_the_copy_survives_the_flush_and_each_arriving_tag_is_hinted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("dest/o")).unwrap();
    git(root, &["init", "-q", "--bare", "src.git"]);
    git(root, &["init", "-q", "--bare", "dest/o/r.git"]);
    git(root, &["init", "-q", "work"]);
    let work = root.join("work");
    git(&work, &["commit", "-q", "--allow-empty", "-m", "old"]);
    git(
        &work,
        &[
            "push",
            "-q",
            "../dest/o/r.git",
            "HEAD:refs/heads/main",
            "HEAD:refs/heads/cforge/gitlab-7",
        ],
    );
    let relay = git(
        root,
        &["-C", "dest/o/r.git", "rev-parse", "cforge/gitlab-7"],
    );
    git(&work, &["commit", "-q", "--allow-empty", "-m", "new"]);
    git(&work, &["tag", "v1"]);
    git(
        &work,
        &[
            "push",
            "-q",
            "../src.git",
            "HEAD:refs/heads/main",
            "HEAD:refs/heads/cforge/own",
            "refs/tags/v1",
        ],
    );

    // curl stands in for the bridge: it records its arguments and the signature it was given, and answers 202.
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = root.join("curl");
    fs::write(
        bin.join("curl"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{0}.args'\nprintf '%s' \"$CFRG_SYNC_HINT_SIGNATURE\" > '{0}.sig'\nprintf 202\n",
            log.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(bin.join("curl"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(
        root.join("cfg.json"),
        serde_json::json!({
            "repos": {"o/r": {"lane": "bulk", "source": root.join("src.git")}},
            "state_dir": root.join("state"),
            "paused_dir": root.join("paused"),
            "kill_switch": root.join("kill"),
            "quiet_fill": false,
            "sync_hint": {"url": "https://bridge.test/hooks/sync", "key_env": "CFRG_TEST_HINT_KEY"}
        })
        .to_string(),
    )
    .unwrap();
    let flush = |args: &[&str]| -> serde_json::Value {
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let out = Command::new(env!("CARGO_BIN_EXE_cfrg"))
            .current_dir(root)
            .args(["gate", "--config", "cfg.json", "--origin"])
            .arg(format!("file://{}", root.join("dest").display()))
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("CFRG_GITHUB_TOKEN", "dummy")
            .env("CFRG_TEST_HINT_KEY", "k")
            .env("PATH", path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    flush(&["enqueue", "o/r"]);
    let done = flush(&["flush", "--execute"]);
    assert_eq!(done["flush"][0]["verdict"], "pushed", "{done}");
    assert_eq!(
        done["flush"][0]["sync_hints"],
        serde_json::json!([{"tag": "v1", "status": 202}])
    );
    let dest = |rev: &str| {
        git(
            root,
            &["-C", "dest/o/r.git", "rev-parse", "--verify", "-q", rev],
        )
    };
    assert_eq!(
        dest("main"),
        git(root, &["-C", "src.git", "rev-parse", "main"])
    );
    assert_eq!(dest("v1"), git(root, &["-C", "src.git", "rev-parse", "v1"]));
    assert_eq!(
        dest("cforge/gitlab-7"),
        relay,
        "the relay head is untouched"
    );
    assert!(
        git(
            root,
            &[
                "-C",
                "dest/o/r.git",
                "for-each-ref",
                "refs/heads/cforge/own"
            ]
        )
        .is_empty(),
        "a primary branch in the relay namespace is never pushed"
    );

    // The hint is the bridge's interface: the body names repository and tag, the signature is its HMAC with the key.
    let args = fs::read_to_string(format!("{}.args", log.display())).unwrap();
    let args: Vec<&str> = args.lines().collect();
    let body = args[args.iter().position(|a| *a == "--data-binary").unwrap() + 1];
    assert_eq!(body, r#"{"repository":"o/r","tag":"v1"}"#);
    assert_eq!(*args.last().unwrap(), "https://bridge.test/hooks/sync");
    let sig = fs::read_to_string(format!("{}.sig", log.display())).unwrap();
    assert!(cfrg::serve::verify_signature(b"k", body.as_bytes(), &sig));
    assert!(
        !args.iter().any(|a| a.contains(&sig)),
        "the signature never enters argv"
    );

    // The relay head does not count: the next flush finds the repository in sync instead of pushing it again.
    flush(&["enqueue", "o/r"]);
    let again = flush(&["flush", "--execute"]);
    assert_eq!(again["flush"][0]["verdict"], "in sync, dequeued", "{again}");
}
