use super::*;
use crate::model::Forge;

#[derive(Default)]
struct Fake {
    calls: Vec<(String, Option<Value>)>,
    code: u16,
    absent: bool,
    wrong_sha: bool,
}
impl Transport for Fake {
    fn request(&mut self, spec: &RequestSpec) -> Result<(u16, Value)> {
        self.calls.push((spec.path.clone(), spec.body.clone()));
        if spec.body.is_some() {
            return Ok((if self.code == 0 { 201 } else { self.code }, json!({})));
        }
        Ok((
            if self.absent { 404 } else { 200 },
            json!({"sha":if self.wrong_sha {"b"} else {"a"}.repeat(40)}),
        ))
    }
}

struct FakeTarget;
impl StatusTarget for FakeTarget {
    fn validate(&self, _origin: &str, _repository: &str) -> Result<()> {
        Ok(())
    }
    fn commit_path(&self, repository: &str, sha: &str) -> String {
        format!("commit-check/{repository}/{sha}")
    }
    fn status_path(&self, repository: &str, sha: &str) -> String {
        format!("post-status/{repository}/{sha}")
    }
    fn auth(&self) -> AuthScheme {
        AuthScheme::Token
    }
    fn status_body(&self, name: &str, _url: &str, state: State) -> Value {
        json!({"name": name, "state": format!("{state:?}")})
    }
    fn commit_field(&self) -> &'static str {
        "sha"
    }
}

static FAKE_TARGET: FakeTarget = FakeTarget;
fn targets() -> StatusTargets {
    StatusTargets {
        forgejo: &FAKE_TARGET,
        gitlab: &FAKE_TARGET,
        bitbucket: &FAKE_TARGET,
    }
}

fn config() -> Config {
    Config {
        schema: 1,
        targets: vec![
            Target {
                provider: Forge::Forgejo,
                origin: "https://forge.example".into(),
                repository: "team/repo".into(),
                token_env: "CFRG_STATUS_FORGEJO_TOKEN".into(),
            },
            Target {
                provider: Forge::Gitlab,
                origin: "https://gitlab.example".into(),
                repository: "group/sub/repo".into(),
                token_env: "CFRG_STATUS_GITLAB_TOKEN".into(),
            },
            Target {
                provider: Forge::Bitbucket,
                origin: "https://api.bitbucket.org".into(),
                repository: "workspace/repo".into(),
                token_env: "CFRG_STATUS_BITBUCKET_TOKEN".into(),
            },
        ],
    }
}
fn options() -> Options {
    Options {
        config: PathBuf::new(),
        state_dir: PathBuf::new(),
        commit: "a".repeat(40),
        name: "ccid/verify".into(),
        url: "https://ci.example/runs/1".into(),
        state: State::Pending,
        started: 100,
    }
}

#[test]
fn provider_payloads_exact_commit_and_idempotent_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path().into()).unwrap();
    let mut fake = Fake::default();
    let mut opts = options();
    let cfg = config();
    let targets = targets();
    assert_eq!(
        report(&mut fake, &journal, &cfg, &opts, &targets).unwrap()["complete"],
        true
    );
    assert!(fake.calls[2].0.contains("group/sub/repo"));
    assert_eq!(fake.calls[1].1.as_ref().unwrap()["name"], "ccid/verify");
    assert_eq!(fake.calls[5].1.as_ref().unwrap()["state"], "Pending");
    report(&mut fake, &journal, &cfg, &opts, &targets).unwrap();
    assert_eq!(fake.calls.len(), 6);
    opts.state = State::Failure;
    report(&mut fake, &journal, &cfg, &opts, &targets).unwrap();
    assert_eq!(fake.calls[9].1.as_ref().unwrap()["state"], "Failure");
    assert_eq!(fake.calls[11].1.as_ref().unwrap()["state"], "Failure");
    opts.state = State::Pending;
    assert_eq!(
        report(&mut fake, &journal, &cfg, &opts, &targets).unwrap()["complete"],
        false
    );
    assert_eq!(fake.calls.len(), 12);
    opts.started = 99;
    assert_eq!(
        report(&mut fake, &journal, &cfg, &opts, &targets).unwrap()["results"][0]["status"],
        "superseded"
    );
}

#[test]
fn absent_and_wrong_commit_never_receive_status() {
    for absent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().into()).unwrap();
        let mut fake = Fake {
            absent,
            wrong_sha: true,
            ..Default::default()
        };
        let result = report(&mut fake, &journal, &config(), &options(), &targets()).unwrap();
        assert_eq!(result["complete"], absent);
        assert_eq!(fake.calls.len(), 3);
        assert!(fake.calls.iter().all(|(_, body)| body.is_none()));
    }
}

#[test]
fn failed_post_is_visible_other_targets_continue_and_uncertainty_blocks_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::open(dir.path().into()).unwrap();
    let mut fake = Fake {
        code: 429,
        ..Default::default()
    };
    assert_eq!(
        report(&mut fake, &journal, &config(), &options(), &targets()).unwrap()["complete"],
        false
    );
    assert_eq!(fake.calls.len(), 6);
    fake.code = 201;
    assert_eq!(
        report(&mut fake, &journal, &config(), &options(), &targets()).unwrap()["complete"],
        false
    );
    assert_eq!(fake.calls.iter().filter(|(_, b)| b.is_some()).count(), 3);
}

#[test]
fn reject_github_credentials_in_urls_and_untrusted_token_names() {
    assert!(origin("https://api.github.com").is_err());
    assert!(origin("https://token@forge.example").is_err());
    assert!(!supported(&Forge::Github));
    assert!(supported(&Forge::Forgejo));
    assert!(validate_credential_name("HOME").is_err());
    assert!(validate_credential_name("CFRG_STATUS_TOKEN").is_ok());
}

#[test]
fn concurrent_reporters_wait_for_the_journal_instead_of_failing() {
    let dir = tempfile::tempdir().unwrap();
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let root = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let journal = Journal::open(root).unwrap();
                let n: u64 = journal.load("counter").unwrap().unwrap_or_default();
                std::thread::sleep(Duration::from_millis(30));
                journal.save("counter", &(n + 1)).unwrap();
                i
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let journal = Journal::open(dir.path().into()).unwrap();
    assert_eq!(journal.load::<u64>("counter").unwrap(), Some(8));
}

#[test]
fn busy_journal_fails_only_after_the_bounded_wait() {
    let dir = tempfile::tempdir().unwrap();
    let _held = Journal::open(dir.path().into()).unwrap();
    let started = Instant::now();
    assert!(Journal::open_waiting(dir.path().into(), Duration::from_millis(200)).is_err());
    assert!(started.elapsed() >= Duration::from_millis(200));
}
