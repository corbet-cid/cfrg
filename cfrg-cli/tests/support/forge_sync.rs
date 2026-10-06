use serde_json::{json, Value};
use std::{
    fs,
    net::TcpListener,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

pub fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

pub struct Fixture {
    pub root: tempfile::TempDir,
    pub source: PathBuf,
    pub replica: PathBuf,
    pub policy: PathBuf,
    daemon: Child,
    port: u16,
}

impl Fixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.git");
        let replica = root.path().join("replica.git");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&replica).unwrap();
        git(&source, &["init", "-qb", "main"]);
        git(&source, &["config", "user.name", "Fixture"]);
        git(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        );
        git(&source, &["config", "commit.gpgsign", "false"]);
        git(&source, &["config", "tag.gpgsign", "false"]);
        git(&replica, &["init", "--bare", "-q"]);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let daemon = Command::new("git")
            .process_group(0)
            .args([
                "daemon",
                "--reuseaddr",
                "--export-all",
                "--enable=receive-pack",
                "--listen=127.0.0.1",
            ])
            .arg(format!("--port={port}"))
            .arg(format!("--base-path={}", root.path().display()))
            .arg(root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < until, "git daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        let policy = root.path().join("policy.json");
        fs::write(&policy, serde_json::to_vec(&json!({
            "schema":1,"free_only":true,
            "forges":{"source":{"kind":"gitlab","url":"https://source.example"},"replica":{"kind":"forgejo","url":"https://replica.example"},"offline":{"kind":"github","url":"https://offline.example"}},
            "ci":{"build":{"driver":"crow","forge":"source","execution":"owned","capabilities":["linux-x86_64"]}},
            "repositories":{"widget":{"ci":"build","visibility":"private","sensitive":true,"locations":{"source":"team/source","replica":"team/replica","offline":"team/missing"},"clone_fallbacks":["replica","offline"],"promotion":"manual"}}
        })).unwrap()).unwrap();
        Self {
            root,
            source,
            replica,
            policy,
            daemon,
            port,
        }
    }

    pub fn commit(&self, text: &str) -> String {
        fs::write(self.source.join("value"), text).unwrap();
        git(&self.source, &["add", "value"]);
        git(&self.source, &["commit", "-qm", text]);
        git(&self.source, &["rev-parse", "HEAD"])
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cfrg"));
        command
            .args(["sync", "--policy"])
            .arg(&self.policy)
            .args(["--repository", "widget", "--timeout", "15"])
            .args(args)
            .env("GIT_CONFIG_COUNT", "4")
            .env("GIT_CONFIG_KEY_3", "protocol.git.allow")
            .env("GIT_CONFIG_VALUE_3", "always")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        for (index, name) in ["source", "replica", "offline"].iter().enumerate() {
            command
                .env(
                    format!("GIT_CONFIG_KEY_{index}"),
                    format!("url.git://127.0.0.1:{}/.insteadOf", self.port),
                )
                .env(
                    format!("GIT_CONFIG_VALUE_{index}"),
                    format!("https://{name}.example/team/"),
                );
        }
        command
    }

    pub fn report(&self, args: &[&str]) -> (Output, Value) {
        let output = self.run(args);
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{error}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output, value)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", "--", &format!("-{}", self.daemon.id())])
            .status();
        let _ = self.daemon.wait();
    }
}
