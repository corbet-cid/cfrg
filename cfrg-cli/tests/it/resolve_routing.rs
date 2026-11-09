//! Real-git routing proof: `cfrg resolve` over a scratch filesystem store,
//! then `git ls-remote --get-url` for every declared and adversarial URL
//! form. No network; fixtures are scratch bare repos under a tempdir.
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
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

fn cfrg(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_cfrg"))
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
}

struct Fixture {
    root: tempfile::TempDir,
    store: PathBuf,
    sha: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        fs::create_dir_all(store.join("acme")).unwrap();
        let work = root.path().join("work");
        fs::create_dir(&work).unwrap();
        git(&work, &["init", "-qb", "main"]);
        fs::write(work.join("value"), "fixture").unwrap();
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
                "fixture",
            ],
        );
        let sha = git(&work, &["rev-parse", "HEAD"]);
        git(
            &work,
            &[
                "clone",
                "--bare",
                "--",
                &work.display().to_string(),
                &store.join("acme/widget.git").display().to_string(),
            ],
        );
        // Neighbor with a non-Forgejo primary: bare repo so the path exists,
        // but resolution must stay canonical regardless.
        git(
            &work,
            &[
                "clone",
                "--bare",
                "--",
                &work.display().to_string(),
                &store.join("acme/widget-evil.git").display().to_string(),
            ],
        );
        Self { root, store, sha }
    }

    fn request(&self) -> Value {
        let store = self.store.display().to_string();
        json!({
            "schema": 1,
            "canonical_base": "https://pointer.example",
            "aliases": [{"url_prefix": "https://github.com/acme", "canonical_owner": "acme"}],
            "repositories": [
                {"id": "widget", "path": "acme/widget",
                 "ref": {"pinned": self.sha.clone()},
                 "primary": "forgejo",
                 "source_urls": ["https://pointer.example/acme/widget",
                                 "https://pointer.example/acme/widget.git",
                                 "https://github.com/acme/widget",
                                 "https://github.com/acme/widget.git"]},
                {"id": "evil", "path": "acme/widget-evil",
                 "ref": {"moving": "refs/heads/main"},
                 "primary": "github",
                 "source_urls": ["https://pointer.example/acme/widget-evil",
                                 "https://pointer.example/acme/widget-evil.git",
                                 "https://github.com/acme/widget-evil",
                                 "https://github.com/acme/widget-evil.git"]},
                {"id": "dotevil", "path": "acme/widget.git-evil",
                 "ref": {"moving": "refs/heads/main"},
                 "primary": "github",
                 "source_urls": ["https://pointer.example/acme/widget.git-evil",
                                 "https://pointer.example/acme/widget.git-evil.git",
                                 "https://github.com/acme/widget.git-evil",
                                 "https://github.com/acme/widget.git-evil.git"]}
            ],
            "stores": [{"kind": "filesystem", "location": store,
                        "identity": "forgejo", "scope": ["acme"],
                        "trusted_single_user": true}],
            "timeout_secs": 20
        })
    }
}

/// `git ls-remote --get-url` applies insteadOf with no network.
fn get_url(config: &Path, url: &str) -> String {
    let output = Command::new("git")
        .args(["ls-remote", "--get-url", "--", url])
        .env("GIT_CONFIG_GLOBAL", config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "get-url {url}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn declared_forms_route_and_guards_hold_under_real_git() {
    let fixture = Fixture::new();
    let request = fixture.root.path().join("request.json");
    fs::write(&request, serde_json::to_vec(&fixture.request()).unwrap()).unwrap();
    let output = cfrg(&["resolve", "--request", &request.display().to_string()]);
    assert!(
        output.status.success(),
        "resolve: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["decisions"][0]["outcome"], "routed");
    assert_eq!(response["decisions"][1]["outcome"], "canonical-pointer");
    assert_eq!(response["decisions"][2]["outcome"], "canonical-pointer");
    let config_path = fixture.root.path().join("job.gitconfig");
    fs::write(&config_path, response["git_config"].as_str().unwrap()).unwrap();

    let via = format!("file://{}/acme/widget.git", fixture.store.display());
    // Every declared routed form reaches the verified store.
    for url in [
        "https://pointer.example/acme/widget",
        "https://pointer.example/acme/widget.git",
        "https://github.com/acme/widget",
        "https://github.com/acme/widget.git",
    ] {
        assert_eq!(get_url(&config_path, url), via, "{url}");
    }
    // Real fetch suffixes survive on the store URL.
    assert_eq!(
        get_url(
            &config_path,
            "https://pointer.example/acme/widget/info/refs?service=git-upload-pack"
        ),
        format!("{via}/info/refs?service=git-upload-pack")
    );
    // Declared neighbor forms stay under the canonical pointer (longest
    // match beats the shorter routed key).
    for (url, expected) in [
        (
            "https://pointer.example/acme/widget-evil",
            "https://pointer.example/acme/widget-evil",
        ),
        (
            "https://pointer.example/acme/widget-evil.git",
            "https://pointer.example/acme/widget-evil",
        ),
        (
            "https://github.com/acme/widget-evil",
            "https://pointer.example/acme/widget-evil",
        ),
        (
            "https://github.com/acme/widget-evil.git",
            "https://pointer.example/acme/widget-evil",
        ),
        (
            // Longest match is the `.git` guard key: stays under the
            // canonical pointer (safe fetch failure), never the store.
            "https://pointer.example/acme/widget-evil.git-evil",
            "https://pointer.example/acme/widget-evil-evil",
        ),
    ] {
        assert_eq!(get_url(&config_path, url), expected, "{url}");
    }
    // Declared `.git`-suffixed neighbor: its own bare identity key guards
    // it against the shorter routed `.../widget.git` key.
    for (url, expected) in [
        (
            "https://pointer.example/acme/widget.git-evil",
            "https://pointer.example/acme/widget.git-evil",
        ),
        (
            "https://pointer.example/acme/widget.git-evil.git",
            "https://pointer.example/acme/widget.git-evil",
        ),
        (
            "https://github.com/acme/widget.git-evil",
            "https://pointer.example/acme/widget.git-evil",
        ),
        (
            "https://github.com/acme/widget.git-evil.git",
            "https://pointer.example/acme/widget.git-evil",
        ),
    ] {
        assert_eq!(get_url(&config_path, url), expected, "{url}");
    }
    // Unrelated owners are untouched.
    for url in [
        "https://pointer.example/acme-evil/x",
        "https://github.com/acme-evil/x",
    ] {
        assert_eq!(get_url(&config_path, url), url, "{url}");
    }
    // Undeclared suffix trick on the routed name matches the shorter routed
    // key (raw prefix): documented limitation. It stays within the declared
    // store path (fetch 404), never a third host, never neighbor content.
    // (`.../widget.git-evil` itself is DECLARED above and stays canonical.)
    let trick = get_url(
        &config_path,
        "https://pointer.example/acme/widget.git-undeclared",
    );
    assert!(
        trick.starts_with(&format!(
            "file://{}/acme/widget.git",
            fixture.store.display()
        )),
        "{trick}"
    );
}
