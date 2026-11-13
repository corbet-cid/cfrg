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
    gate_with_path(dir, args, token, None)
}

/// `gate`, with `bin` (a directory of stand-in executables) in front of PATH.
fn gate_with_path(dir: &Path, args: &[&str], token: bool, bin: Option<&Path>) -> serde_json::Value {
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
    if let Some(bin) = bin {
        command.env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        );
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

/// One repository `o/r`: a bare source with `main` at p1 and p2 (tag `v1` on p2), an empty bare destination, and a work clone.
struct Fixture {
    root: std::path::PathBuf,
    work: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::create_dir_all(root.join("dest/o")).unwrap();
    git(&root, &["init", "-q", "--bare", "src.git"]);
    git(&root, &["init", "-q", "--bare", "dest/o/r.git"]);
    git(&root, &["init", "-q", "work"]);
    let work = root.join("work");
    git(&work, &["commit", "-q", "--allow-empty", "-m", "p1"]);
    git(&work, &["commit", "-q", "--allow-empty", "-m", "p2"]);
    git(&work, &["tag", "v1"]);
    git(
        &work,
        &[
            "push",
            "-q",
            "../src.git",
            "HEAD:refs/heads/main",
            "refs/tags/v1",
        ],
    );
    Fixture {
        root,
        work,
        _dir: dir,
    }
}

impl Fixture {
    fn write_config(&self, extra: serde_json::Value, repo_extra: serde_json::Value) {
        let mut repo = serde_json::json!({"lane": "bulk", "source": self.root.join("src.git")});
        for (k, v) in repo_extra.as_object().into_iter().flatten() {
            repo[k] = v.clone();
        }
        let mut config = serde_json::json!({
            "repos": {"o/r": repo},
            "budgets": {"repo_min_gap_secs": 0},
            "state_dir": self.root.join("state"),
            "paused_dir": self.root.join("paused"),
            "kill_switch": self.root.join("kill"),
            "quiet_fill": false
        });
        for (k, v) in extra.as_object().into_iter().flatten() {
            config[k] = v.clone();
        }
        fs::write(self.root.join("cfg.json"), config.to_string()).unwrap();
    }

    fn flush(&self) -> serde_json::Value {
        gate(&self.root, &["enqueue", "o/r"], false);
        gate(&self.root, &["flush", "--execute"], true)
    }

    fn status(&self) -> serde_json::Value {
        gate(&self.root, &["status"], false)
    }

    /// Refnames of the destination, one per line, sorted.
    fn dest_refs(&self) -> Vec<String> {
        git(
            &self.root,
            &["-C", "dest/o/r.git", "for-each-ref", "--format=%(refname)"],
        )
        .lines()
        .map(String::from)
        .collect()
    }

    fn rev(&self, repo: &str, rev: &str) -> String {
        git(&self.root, &["-C", repo, "rev-parse", rev])
            .trim()
            .to_string()
    }
}

/// Points 2 and R3: a copy whose history the primary rewrote cannot be brought over by a push without force. It is held and
/// reported as diverged, no push slot is spent on it (not now, not on the next flush), and the hold clears itself once the copy
/// has been reconciled.
#[test]
fn a_diverged_copy_is_held_and_reported_without_spending_a_push_and_clears_when_reconciled() {
    let f = fixture();
    // The copy holds the old history (unrelated to the primary's now): `main` and the tag `v1` stand at other objects.
    git(&f.root, &["init", "-q", "old"]);
    let old = f.root.join("old");
    git(&old, &["commit", "-q", "--allow-empty", "-m", "old"]);
    git(&old, &["tag", "v1"]);
    git(
        &old,
        &[
            "push",
            "-q",
            "../dest/o/r.git",
            "HEAD:refs/heads/main",
            "refs/tags/v1",
        ],
    );
    let before = f.rev("dest/o/r.git", "main");
    f.write_config(serde_json::json!({}), serde_json::json!({}));

    let held = f.flush();
    let verdict = held["flush"][0]["verdict"].as_str().unwrap();
    assert!(
        verdict.starts_with("held: diverged from the primary"),
        "{held}"
    );
    let status = f.status();
    assert_eq!(
        status["pushes"]["hour"], 0,
        "no push slot is spent: {status}"
    );
    assert_eq!(status["diverged"]["o/r"]["hold"], "non-fast-forward");
    assert_eq!(
        status["diverged"]["o/r"]["refs"],
        serde_json::json!(["refs/heads/main", "refs/tags/v1"])
    );
    assert_eq!(
        f.rev("dest/o/r.git", "main"),
        before,
        "the copy is untouched"
    );

    // The next flush finds the same divergence and still pushes nothing.
    let again = f.flush();
    assert!(again["flush"][0]["verdict"]
        .as_str()
        .unwrap()
        .starts_with("held: diverged"));
    assert_eq!(f.status()["pushes"]["hour"], 0, "no retry, no budget");

    // A person reconciles the copy with the primary: the hold clears itself and the repository is in sync.
    git(
        &f.work,
        &[
            "push",
            "-q",
            "-f",
            "../dest/o/r.git",
            "HEAD:refs/heads/main",
            "refs/tags/v1",
        ],
    );
    let synced = f.flush();
    assert_eq!(
        synced["flush"][0]["verdict"], "in sync, dequeued",
        "{synced}"
    );
    assert_eq!(f.status()["diverged"], serde_json::json!({}));
    assert_eq!(f.status()["pushes"]["hour"], 0);
}

/// Point 1 (atomic) and the R30 side of point 2: a ref the destination refuses takes every other ref of the push down with it,
/// and the destination's rejection is not repeated on a clock, only after a ref changed.
#[test]
fn a_push_is_atomic_and_a_rejected_one_is_not_repeated_until_a_ref_changes() {
    let f = fixture();
    git(
        &f.work,
        &["push", "-q", "../src.git", "HEAD:refs/heads/blocked"],
    );
    // The destination refuses one branch, as GitHub does for a protected one (GH006) or a push-protection hit.
    let hook = f.root.join("dest/o/r.git/hooks/update");
    fs::write(
        &hook,
        "#!/bin/sh\n[ \"$1\" = refs/heads/blocked ] && { echo 'blocked by policy' >&2; exit 1; }\nexit 0\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    f.write_config(serde_json::json!({}), serde_json::json!({}));

    let first = f.flush();
    assert_eq!(
        first["flush"][0]["verdict"], "push rejected; held until a ref changes",
        "{first}"
    );
    assert!(
        f.dest_refs().is_empty(),
        "atomic: neither main nor the tag landed beside the refused branch: {:?}",
        f.dest_refs()
    );
    let status = f.status();
    assert_eq!(status["pushes"]["hour"], 1);
    assert_eq!(status["diverged"]["o/r"]["hold"], "push-rejected");

    // The same push is not tried again: no slot, no request.
    let second = f.flush();
    assert!(
        second["flush"][0]["verdict"]
            .as_str()
            .unwrap()
            .starts_with("held: the destination rejected this exact push"),
        "{second}"
    );
    assert_eq!(f.status()["pushes"]["hour"], 1, "no retry on a clock");

    // The primary changes (the refused branch is gone): that is a different push, and it lands.
    git(
        &f.root,
        &["-C", "src.git", "update-ref", "-d", "refs/heads/blocked"],
    );
    let third = f.flush();
    assert_eq!(third["flush"][0]["verdict"], "pushed", "{third}");
    assert_eq!(f.rev("dest/o/r.git", "main"), f.rev("src.git", "main"));
    let status = f.status();
    assert_eq!(status["pushes"]["hour"], 2);
    assert_eq!(status["diverged"], serde_json::json!({}));
}

/// Point 4 (R3): a copy-only branch is deleted by the push, and what is not the gate's to delete survives: the forge router's
/// relay head, a pull-request ref, a copy-only tag and the destination's default branch (a bare repository refuses to delete the
/// current branch the way GitHub refuses its default, which would fail the whole atomic push).
#[test]
fn a_copy_only_branch_is_pruned_next_to_a_surviving_relay_head_pull_ref_tag_and_default_branch() {
    let f = fixture();
    git(&f.work, &["tag", "gh-only", "HEAD~1"]);
    git(
        &f.work,
        &[
            "push",
            "-q",
            "../dest/o/r.git",
            "HEAD~1:refs/heads/main",
            "HEAD~1:refs/heads/stale",
            "HEAD~1:refs/heads/legacy",
            "HEAD~1:refs/heads/cforge/gitlab-7",
            "HEAD~1:refs/pull/1/head",
            "refs/tags/gh-only",
        ],
    );
    git(
        &f.root,
        &[
            "-C",
            "dest/o/r.git",
            "symbolic-ref",
            "HEAD",
            "refs/heads/legacy",
        ],
    );
    let relay = f.rev("dest/o/r.git", "cforge/gitlab-7");
    f.write_config(serde_json::json!({}), serde_json::json!({}));

    gate(&f.root, &["enqueue", "o/r"], false);
    let dry = gate(&f.root, &["flush"], false);
    assert_eq!(dry["flush"][0]["verdict"], "would push", "{dry}");
    assert_eq!(
        dry["flush"][0]["pruned"],
        serde_json::json!(["refs/heads/stale"])
    );
    assert!(
        f.dest_refs().contains(&"refs/heads/stale".to_string()),
        "a dry run deletes nothing"
    );

    let done = gate(&f.root, &["flush", "--execute"], true);
    assert_eq!(done["flush"][0]["verdict"], "pushed", "{done}");
    assert_eq!(
        done["flush"][0]["pruned"],
        serde_json::json!(["refs/heads/stale"])
    );
    assert_eq!(
        f.dest_refs(),
        [
            "refs/heads/cforge/gitlab-7",
            "refs/heads/legacy",
            "refs/heads/main",
            "refs/pull/1/head",
            "refs/tags/gh-only",
            "refs/tags/v1",
        ],
        "stale is gone; the relay head, the pull ref, the copy-only tag and the default branch survive"
    );
    assert_eq!(f.rev("dest/o/r.git", "main"), f.rev("src.git", "main"));
    assert_eq!(
        f.rev("dest/o/r.git", "cforge/gitlab-7"),
        relay,
        "the relay head is untouched"
    );

    // What is left over (a copy-only tag, the default branch, the relay head) does not keep the repository out of sync.
    let again = f.flush();
    assert_eq!(again["flush"][0]["verdict"], "in sync, dequeued", "{again}");
    assert_eq!(f.status()["pushes"]["hour"], 1);
}

/// A source that lost branches by accident must never empty a destination: more copy-only branches than `max_prune` hold the
/// repository, untouched, until a person decides (here: raises the limit).
#[test]
fn more_copy_only_branches_than_max_prune_hold_the_repository_untouched() {
    let f = fixture();
    git(
        &f.work,
        &[
            "push",
            "-q",
            "../dest/o/r.git",
            "HEAD~1:refs/heads/main",
            "HEAD~1:refs/heads/gone-1",
            "HEAD~1:refs/heads/gone-2",
        ],
    );
    f.write_config(serde_json::json!({"max_prune": 1}), serde_json::json!({}));
    let held = f.flush();
    assert!(
        held["flush"][0]["verdict"]
            .as_str()
            .unwrap()
            .starts_with("held: diverged from the primary (more copy-only"),
        "{held}"
    );
    assert_eq!(f.status()["diverged"]["o/r"]["hold"], "prune-limit");
    assert_eq!(f.status()["pushes"]["hour"], 0);
    assert_eq!(
        f.dest_refs().len(),
        3,
        "nothing was deleted: {:?}",
        f.dest_refs()
    );

    f.write_config(serde_json::json!({"max_prune": 2}), serde_json::json!({}));
    let done = f.flush();
    assert_eq!(done["flush"][0]["verdict"], "pushed", "{done}");
    assert_eq!(f.dest_refs(), ["refs/heads/main", "refs/tags/v1"]);
    assert_eq!(f.status()["diverged"], serde_json::json!({}));
}

/// A source with no branch at all pushes nothing and prunes nothing, whatever the destination holds.
#[test]
fn a_source_without_branches_never_prunes_a_destination() {
    let f = fixture();
    git(
        &f.work,
        &["push", "-q", "../dest/o/r.git", "HEAD:refs/heads/main"],
    );
    git(
        &f.root,
        &["-C", "src.git", "update-ref", "-d", "refs/heads/main"],
    );
    f.write_config(serde_json::json!({}), serde_json::json!({}));
    let done = f.flush();
    assert_eq!(
        done["flush"][0]["verdict"], "source has no branches yet",
        "{done}"
    );
    assert_eq!(f.dest_refs(), ["refs/heads/main"]);
}

/// Point 3 (R4): LFS payloads are not replicated by default; only a repository that declares `"lfs": "payload"` gets them. A
/// stand-in `git-lfs` records every call and lists one object.
#[test]
fn lfs_payloads_are_never_uploaded_unless_the_repository_declares_the_exception() {
    let f = fixture();
    let bin = f.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = f.root.join("lfs.log");
    let oid = "ab".repeat(32);
    fs::write(
        bin.join("git-lfs"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$1\" in ls-files) echo '{oid} * big.bin';; esac\n",
            log.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(bin.join("git-lfs"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(&f.root, &["init", "-q", "--bare", "dest/o/p.git"]);
    let source = f.root.join("src.git");
    // `git lfs install` writes a pre-push hook that uploads the payloads: a source that carries one must not make a pointers-only
    // repository upload anything (the hook here refuses the push, so a push that ran it would fail).
    let hook = source.join("hooks/pre-push");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(
        f.root.join("cfg.json"),
        serde_json::json!({
            "repos": {
                "o/r": {"lane": "bulk", "source": source},
                "o/p": {"lane": "bulk", "source": source, "lfs": "payload"}
            },
            "budgets": {"repo_min_gap_secs": 0},
            "state_dir": f.root.join("state"),
            "paused_dir": f.root.join("paused"),
            "kill_switch": f.root.join("kill"),
            "quiet_fill": false
        })
        .to_string(),
    )
    .unwrap();
    gate(&f.root, &["enqueue", "o/r", "o/p"], false);
    let done = gate_with_path(&f.root, &["flush", "--execute"], true, Some(&bin));
    let verdicts: Vec<_> = done["flush"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["verdict"].as_str().unwrap())
        .collect();
    assert_eq!(verdicts, ["pushed", "pushed"], "{done}");
    // Both copies carry the refs; the payload goes only where it was declared.
    assert_eq!(f.rev("dest/o/r.git", "main"), f.rev("src.git", "main"));
    assert_eq!(f.rev("dest/o/p.git", "main"), f.rev("src.git", "main"));
    let calls = fs::read_to_string(&log).unwrap();
    let calls: Vec<&str> = calls.lines().collect();
    let url = format!("file://{}/o/p.git", f.root.join("dest").display());
    assert_eq!(
        calls,
        [
            "ls-files --all --long".to_string(),
            format!("push --object-id {url} {oid}")
        ],
        "git-lfs is called for the declared repository and for nothing else"
    );
}

/// A push that fails in transport lists no refused ref, so it is NOT a rejection: nothing is held, and the next flush tries again
/// (one slot each). Exit status cannot tell the two apart: a remote helper that dies in the middle of a push makes `git push` exit
/// 1, like a refusal does. The stand-in helper `git-remote-gatex` lists the destination's refs and dies on `push`.
#[test]
fn a_push_that_dies_in_transport_is_tried_again_and_not_held_as_rejected() {
    let f = fixture();
    let bin = f.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let helper = bin.join("git-remote-gatex");
    fs::write(
        &helper,
        "#!/bin/sh\nwhile read cmd rest; do\n  case \"$cmd\" in\n    capabilities) printf 'push\\nfetch\\noption\\n\\n';;\n    option) printf 'ok\\n';;\n    list) git -C \"$FAKE_DEST\" for-each-ref --format='%(objectname) %(refname)'; printf '\\n';;\n    push) while read l; do [ -z \"$l\" ] && break; done; exit 1;;\n    '') exit 0;;\n  esac\ndone\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    }
    f.write_config(serde_json::json!({}), serde_json::json!({}));
    let flush = || -> serde_json::Value {
        gate(&f.root, &["enqueue", "o/r"], false);
        let out = Command::new(env!("CARGO_BIN_EXE_cfrg"))
            .current_dir(&f.root)
            .args(["gate", "--config", "cfg.json", "--origin", "gatex://host"])
            .args(["flush", "--execute"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("CFRG_GITHUB_TOKEN", "dummy")
            .env("FAKE_DEST", f.root.join("dest/o/r.git"))
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let first = flush();
    assert_eq!(first["flush"][0]["verdict"], "push failed", "{first}");
    let status = f.status();
    assert_eq!(
        status["diverged"],
        serde_json::json!({}),
        "not a rejection: {status}"
    );
    assert_eq!(status["pushes"]["hour"], 1);
    // Nothing is held, so the next flush tries the same push again (and spends its slot).
    let second = flush();
    assert_eq!(second["flush"][0]["verdict"], "push failed", "{second}");
    assert_eq!(f.status()["pushes"]["hour"], 2);
}

/// A hold belongs to a listed repository: once the repository leaves the configuration, its record goes with it.
#[test]
fn a_hold_of_a_repository_that_left_the_configuration_is_dropped() {
    let f = fixture();
    git(&f.root, &["init", "-q", "old"]);
    let old = f.root.join("old");
    git(&old, &["commit", "-q", "--allow-empty", "-m", "old"]);
    git(
        &old,
        &["push", "-q", "../dest/o/r.git", "HEAD:refs/heads/main"],
    );
    f.write_config(serde_json::json!({}), serde_json::json!({}));
    f.flush();
    assert!(f.status()["diverged"]["o/r"].is_object());
    // Another repository takes the configuration's place; `o/r` is no longer listed.
    git(&f.root, &["init", "-q", "--bare", "dest/o/s.git"]);
    let config = serde_json::json!({
        "repos": {"o/s": {"lane": "bulk", "source": f.root.join("src.git")}},
        "state_dir": f.root.join("state"),
        "paused_dir": f.root.join("paused"),
        "kill_switch": f.root.join("kill"),
        "quiet_fill": false
    });
    fs::write(f.root.join("cfg.json"), config.to_string()).unwrap();
    gate(&f.root, &["enqueue", "o/s"], false);
    gate(&f.root, &["flush", "--execute"], true);
    assert_eq!(f.status()["diverged"], serde_json::json!({}));
    let kept = fs::read_to_string(f.root.join("state/diverged.json")).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&kept).unwrap(),
        serde_json::json!({})
    );
}

/// The deletion is leased on the object the plan saw: a copy-only branch that moved after the destination was read (somebody pushed to
/// it) is not deleted. The push is refused whole (atomic), the refusal is held for exactly that push, and the next flush, which sees
/// the new object, plans and deletes it. A `git` stand-in moves the branch right before the first `git push`.
#[test]
fn a_copy_only_branch_that_moved_after_the_plan_is_not_deleted() {
    let f = fixture();
    git(&f.work, &["commit", "-q", "--allow-empty", "-m", "extra"]);
    git(
        &f.work,
        &[
            "push",
            "-q",
            "../dest/o/r.git",
            "HEAD~2:refs/heads/main",
            "HEAD~2:refs/heads/stale",
            "HEAD:refs/tags/spare",
        ],
    );
    let moved_to = f.rev("dest/o/r.git", "spare");
    let planned = f.rev("dest/o/r.git", "stale");
    assert_ne!(planned, moved_to);
    let real = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let bin = f.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let wrapper = bin.join("git");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nreal='{real}'\nfor a in \"$@\"; do\n  if [ \"$a\" = push ] && [ ! -e '{mark}' ]; then\n    : > '{mark}'\n    \"$real\" -C '{dest}' update-ref refs/heads/stale '{moved_to}'\n  fi\ndone\nexec \"$real\" \"$@\"\n",
            real = real.trim(),
            mark = f.root.join("moved").display(),
            dest = f.root.join("dest/o/r.git").display(),
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    }
    f.write_config(serde_json::json!({}), serde_json::json!({}));
    gate(&f.root, &["enqueue", "o/r"], false);
    let first = gate_with_path(&f.root, &["flush", "--execute"], true, Some(&bin));
    assert_eq!(
        first["flush"][0]["verdict"], "push rejected; held until a ref changes",
        "{first}"
    );
    assert_eq!(
        f.rev("dest/o/r.git", "stale"),
        moved_to,
        "the moved branch is not deleted"
    );
    assert_ne!(
        f.rev("dest/o/r.git", "main"),
        f.rev("src.git", "main"),
        "atomic: main did not move either"
    );

    // The destination changed, so this is a different push: it is planned against the new object and goes through.
    gate(&f.root, &["enqueue", "o/r"], false);
    let second = gate_with_path(&f.root, &["flush", "--execute"], true, Some(&bin));
    assert_eq!(second["flush"][0]["verdict"], "pushed", "{second}");
    assert_eq!(
        f.dest_refs(),
        ["refs/heads/main", "refs/tags/spare", "refs/tags/v1"]
    );
    assert_eq!(f.rev("dest/o/r.git", "main"), f.rev("src.git", "main"));
}
