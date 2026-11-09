#![cfg(unix)]
use serde_json::json;
use std::{
    fs,
    net::TcpListener,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        // Git's dispatcher can retain a child git-daemon process. Terminate
        // this fixture's entire isolated group, including inherited lock FDs.
        let _ = Command::new("kill")
            .args(["-TERM", "--", &format!("-{}", self.0.id())])
            .status();
        let _ = self.0.wait();
    }
}
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

#[test]
fn exact_clone_falls_back_without_moving_refs_or_overwriting_existing_work() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let repo = root.join("widget");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    fs::write(repo.join("value"), "first").unwrap();
    git(&repo, &["add", "value"]);
    git(
        &repo,
        &["-c", "commit.gpgsign=false", "commit", "-qm", "first"],
    );
    let first = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("value"), "newer").unwrap();
    git(
        &repo,
        &["-c", "commit.gpgsign=false", "commit", "-qam", "newer"],
    );
    let latest = git(&repo, &["rev-parse", "HEAD"]);
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _daemon = Daemon(
        Command::new("git")
            .process_group(0)
            .args([
                "daemon",
                "--reuseaddr",
                "--export-all",
                "--listen=127.0.0.1",
            ])
            .arg(format!("--port={port}"))
            .arg(format!("--base-path={}", root.display()))
            .arg(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let until = Instant::now() + Duration::from_secs(3);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < until, "git daemon failed to start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let policy = root.join("policy.json");
    fs::write(&policy,serde_json::to_vec(&json!({
        "schema":1,"free_only":true,
        "forges":{"a":{"kind":"gitlab","url":"https://primary.example"},"b":{"kind":"bitbucket","url":"https://backup.example"}},
        "ci":{"build":{"driver":"crow","forge":"a","execution":"owned","capabilities":["linux-x86_64"]}},
        "repositories":{"widget":{"ci":"build","visibility":"private","sensitive":true,"locations":{"a":"group/missing","b":"team/widget"},"clone_fallbacks":["b"],"promotion":"manual"}}
    })).unwrap()).unwrap();
    let run = |commit: &str, destination: &Path| {
        Command::new(env!("CARGO_BIN_EXE_cfrg"))
            .args(["clone", "--policy"])
            .arg(&policy)
            .args([
                "--repository",
                "widget",
                "--commit",
                commit,
                "--destination",
            ])
            .arg(destination)
            .args(["--timeout", "5"])
            .env("GIT_CONFIG_COUNT", "2")
            .env(
                "GIT_CONFIG_KEY_0",
                format!("url.git://127.0.0.1:{port}/.insteadOf"),
            )
            .env("GIT_CONFIG_VALUE_0", "https://primary.example/group/")
            .env(
                "GIT_CONFIG_KEY_1",
                format!("url.git://127.0.0.1:{port}/.insteadOf"),
            )
            .env("GIT_CONFIG_VALUE_1", "https://backup.example/team/")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    };
    let destination = root.join("checkout");
    let out = run(&first, &destination);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(git(&destination, &["rev-parse", "HEAD"]), first);
    assert_eq!(
        fs::read_to_string(destination.join("value")).unwrap(),
        "first"
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), latest);
    assert!(!run(&latest, &destination).status.success());
    assert_eq!(
        fs::read_to_string(destination.join("value")).unwrap(),
        "first"
    );
    let missing = root.join("missing");
    assert!(!run(&"f".repeat(40), &missing).status.success());
    assert!(!missing.exists());
    assert!(!run("main", &missing).status.success());
    assert!(!missing.exists());
}
