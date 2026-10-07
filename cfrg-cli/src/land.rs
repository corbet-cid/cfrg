//! `cfrg land`: schedule a landing and let the unattended follower finish it.
//!
//! `cfrg land <repository> <branch>` puts the pushed branch in the repository's
//! queue, runs one pass (a branch that is already green and current lands
//! right away), starts the follower when something is still waiting and
//! returns. The follower (`--follow`, started detached) repeats the same pass
//! until the queue is settled; `--step` runs a single pass for a timer or for
//! `cfrg serve` later. No agent is involved after the push.
use cfrg::{
    land::{self, Entry, Hooks, Journal, Policy, Report},
    model::Forge,
    native::http::{Http, Pacing},
    process::Runner,
    status::State,
    Environment, Result, INTERRUPTED,
};
use clap::Args;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

const PACING: Pacing = Pacing {
    read: 1,
    write: 3,
    creation: 3,
};

#[derive(Args)]
pub struct Options {
    /// Declared landing policy (JSON).
    #[arg(long, env = "CFRG_LAND_POLICY")]
    policy: Option<PathBuf>,
    /// Journal, request state and follower logs; shared by every pass on this host.
    #[arg(long, env = "CFRG_LAND_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// Repository `owner/name`.
    repository: Option<String>,
    /// Pushed branch to land (with a repository, and no mode flag).
    branch: Option<String>,
    /// One idempotent pass over the queue (all declared repositories without one).
    #[arg(long, conflicts_with_all = ["status", "protect", "capabilities", "follow"])]
    step: bool,
    /// Read-only view of the queue and each head's verdict.
    #[arg(long, conflicts_with_all = ["protect", "capabilities", "follow"])]
    status: bool,
    /// Reconcile branch protection so merges require the CI status.
    #[arg(long, conflicts_with_all = ["capabilities", "follow"])]
    protect: bool,
    /// Write changes (protection); without it `--protect` only plans.
    #[arg(long)]
    apply: bool,
    /// Print what each adapter declares for landing and release, then exit.
    #[arg(long, conflicts_with = "follow")]
    capabilities: bool,
    /// Do not start the unattended follower after scheduling.
    #[arg(long)]
    no_follow: bool,
    /// Internal: the detached follower for one repository.
    #[arg(long, hide = true)]
    follow: bool,
}

pub fn run(options: Options) -> Result<()> {
    if options.capabilities {
        println!("{}", serde_json::to_string_pretty(&capabilities())?);
        return Ok(());
    }
    let policy = Policy::load(
        options
            .policy
            .as_deref()
            .ok_or("Set --policy or CFRG_LAND_POLICY")?,
    )?;
    require_land(policy.forge)?;
    let state_dir = options
        .state_dir
        .clone()
        .ok_or("Set --state-dir or CFRG_LAND_STATE_DIR")?;
    fs::create_dir_all(&state_dir)?;
    let repositories = |options: &Options| -> Vec<String> {
        options.repository.clone().map_or_else(
            || policy.repositories.iter().map(|r| r.path.clone()).collect(),
            |one| vec![one],
        )
    };
    if options.protect {
        let mut http = open_http(&state_dir, options.apply)?;
        for repository in repositories(&options) {
            let contexts = policy.contexts(&repository);
            let report = cfgj::land::protect(
                &mut http,
                &policy.endpoint,
                &repository,
                &contexts,
                options.apply,
            )?;
            println!("{report}");
        }
        return Ok(());
    }
    if options.status {
        let mut http = open_http(&state_dir, false)?;
        for repository in repositories(&options) {
            println!("{}", queue_view(&policy, &mut http, &repository)?);
        }
        return Ok(());
    }
    if options.step {
        for repository in repositories(&options) {
            let report = pass(&policy, &state_dir, &repository, None)?;
            print_report(&repository, &report);
        }
        return Ok(());
    }
    let repository = options
        .repository
        .as_deref()
        .ok_or("Give a repository and a branch, or a mode flag")?;
    if options.follow {
        return follow(&policy, &state_dir, repository);
    }
    let branch = options.branch.as_deref().ok_or("Give the branch to land")?;
    let report = pass(&policy, &state_dir, repository, Some(branch))?;
    print_report(repository, &report);
    if report.waiting && !options.no_follow {
        spawn_follower(&options, &state_dir, repository)?;
    }
    Ok(())
}

pub fn capabilities() -> Value {
    let row = |land: cfrg::land::Capability, release: cfrg::land::Capability| json!({"land": land, "release": release});
    json!({
        "forgejo": row(cfgj::land::LAND, cfgj::land::RELEASE),
        "gitlab": row(cglb::LAND, cglb::RELEASE),
        "bitbucket": row(cbkt::LAND, cbkt::RELEASE),
        "github": row(cghb::LAND, cghb::RELEASE),
    })
}

fn require_land(forge: Forge) -> Result<()> {
    let capability = match forge {
        Forge::Forgejo => return Ok(()),
        Forge::Gitlab => cglb::LAND,
        Forge::Bitbucket => cbkt::LAND,
        Forge::Github => cghb::LAND,
    };
    Err(format!(
        "Landing is {:?} for this forge: {}",
        capability.support, capability.note
    )
    .into())
}

fn print_report(repository: &str, report: &Report) {
    for event in &report.events {
        println!("{event}");
    }
    println!(
        "{}",
        json!({"event":"pass","repository":repository,"waiting":report.waiting})
    );
}

/// Open the request state, waiting for a concurrent pass and clearing a lock
/// whose owner no longer exists.
fn open_http(state_dir: &Path, apply: bool) -> Result<Http> {
    let path = state_dir.join("http.json");
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match Http::open(&path, apply) {
            Ok(http) => return Ok(http.paced(PACING)),
            Err(error) if error.to_string().contains("locked") => {
                if !clear_dead_lock(&path.with_extension("lock")) && Instant::now() >= deadline {
                    return Err(error);
                }
                thread::sleep(Duration::from_secs(1));
            }
            Err(error) => return Err(error),
        }
    }
}

/// A lock file names its owner's pid; only an owner that is provably gone
/// (Linux `/proc` present, entry absent) is cleared.
fn clear_dead_lock(lock: &Path) -> bool {
    if !Path::new("/proc/self").exists() {
        return false;
    }
    let Ok(text) = fs::read_to_string(lock) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return false;
    };
    if Path::new(&format!("/proc/{pid}")).exists() {
        return false;
    }
    fs::remove_file(lock).is_ok()
}

struct Hook {
    retest: Vec<String>,
    landed: Vec<String>,
    origin: String,
    token_env: String,
    state_dir: PathBuf,
}

/// Longest a hook command or checkout step may take; hooks only submit work.
const HOOK_TIMEOUT: Duration = Duration::from_secs(600);

impl Hook {
    fn runner(&self, cwd: &Path, with_token: bool) -> Result<Runner> {
        let mut env: Environment = std::env::vars_os()
            .filter(|(key, _)| key != self.token_env.as_str())
            .collect();
        if with_token {
            // Git reads the credential from its environment, never from argv.
            let token =
                std::env::var(&self.token_env).map_err(|_| format!("Set {}", self.token_env))?;
            env.insert("GIT_CONFIG_COUNT".into(), "1".into());
            env.insert("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into());
            env.insert(
                "GIT_CONFIG_VALUE_0".into(),
                format!("Authorization: token {token}").into(),
            );
        }
        Runner::new(cwd.to_path_buf(), env, HOOK_TIMEOUT)
    }

    /// A persistent work clone of the repository, at exactly the entry head.
    /// Hooks run one at a time inside a pass, so the clone is never shared.
    fn checkout(&self, repository: &str, entry: &Entry) -> Result<PathBuf> {
        let dir = self
            .state_dir
            .join("work")
            .join(repository.replace('/', "-"));
        fs::create_dir_all(&dir)?;
        let runner = self.runner(&dir, true)?;
        let git = |args: &[&str]| -> Result<String> {
            let mut argv: Vec<String> = ["git", "-c", "core.hooksPath=/dev/null"]
                .into_iter()
                .map(String::from)
                .collect();
            argv.extend(args.iter().map(|a| a.to_string()));
            runner.run(&argv, true)
        };
        if !dir.join(".git").exists() {
            git(&["init", "--quiet"])?;
        }
        let url = format!("{}/{}.git", self.origin, repository);
        if git(&["remote", "set-url", "origin", url.as_str()]).is_err() {
            git(&["remote", "add", "origin", url.as_str()])?;
        }
        let refspec = format!("+refs/heads/{0}:refs/remotes/origin/{0}", entry.branch);
        git(&["fetch", "--quiet", "--no-tags", "origin", refspec.as_str()])?;
        git(&[
            "checkout",
            "--quiet",
            "--force",
            "--detach",
            entry.head.as_str(),
        ])?;
        git(&["clean", "--quiet", "-fdx"])?;
        Ok(dir)
    }

    /// Run a declared command to completion (hooks only submit work, so this
    /// is quick and bounded); nothing is declared when the template is empty.
    fn start(&self, template: &[String], repository: &str, entry: &Entry) -> Result<bool> {
        if template.is_empty() {
            return Ok(false);
        }
        let (cwd, checkout) = if land::wants_checkout(template) {
            let dir = self.checkout(repository, entry)?;
            (dir.clone(), dir.to_string_lossy().into_owned())
        } else {
            (self.state_dir.clone(), String::new())
        };
        let argv = land::expand(template, &self.origin, repository, entry, &checkout);
        let output = self.runner(&cwd, false)?.run(&argv, true)?;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.state_dir.join("hooks.log"))
            .and_then(|mut log| {
                use std::io::Write;
                writeln!(log, "{} {}@{}: {}", argv[0], repository, entry.head, output)
            })?;
        Ok(true)
    }
}

impl Hooks for Hook {
    fn retest(&mut self, repository: &str, entry: &Entry) -> Result<bool> {
        self.start(&self.retest, repository, entry)
    }
    fn landed(&mut self, repository: &str, entry: &Entry) -> Result<bool> {
        self.start(&self.landed, repository, entry)
    }
}

#[cfg(unix)]
fn detach(mut command: Command) -> Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    Ok(command.spawn()?)
}

#[cfg(not(unix))]
fn detach(_command: Command) -> Result<std::process::Child> {
    Err("Detached follow-up processes need a Unix host".into())
}

/// Run `f` against the repository's forge target with the journal loaded and
/// saved around it. The request state is held only for the duration of `f`.
fn pass(
    policy: &Policy,
    state_dir: &Path,
    repository: &str,
    branch: Option<&str>,
) -> Result<Report> {
    let mut http = open_http(state_dir, true)?;
    let journal_path = state_dir.join("journal.json");
    let mut journal = Journal::load(&journal_path)?;
    let settings = policy.settings(repository);
    let mut hook = Hook {
        retest: policy.retest(repository),
        landed: policy.landed(repository),
        origin: policy.endpoint.origin.clone(),
        token_env: policy.endpoint.token_env.clone(),
        state_dir: state_dir.to_path_buf(),
    };
    let result = (|| {
        let mut target = cfgj::land::Land::new(&policy.endpoint, repository, &mut http)?;
        match branch {
            Some(branch) => land::request(
                &mut target,
                &mut hook,
                &settings,
                &mut journal,
                repository,
                branch,
            ),
            None => land::step(&mut target, &mut hook, &settings, &mut journal, repository),
        }
    })();
    journal.save(&journal_path)?;
    result
}

fn follow(policy: &Policy, state_dir: &Path, repository: &str) -> Result<()> {
    let slug: String = repository
        .chars()
        .map(|c| if c == '/' { '-' } else { c })
        .collect();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state_dir.join(format!("follow-{slug}.lock")))?;
    if lock.try_lock().is_err() {
        println!(
            "{}",
            json!({"event":"follower-running","repository":repository})
        );
        return Ok(());
    }
    let started = Instant::now();
    let mut failures = 0u32;
    loop {
        match pass(policy, state_dir, repository, None) {
            Ok(report) => {
                failures = 0;
                print_report(repository, &report);
                if !report.waiting {
                    return Ok(());
                }
            }
            Err(error) => {
                failures += 1;
                println!(
                    "{}",
                    json!({"event":"pass-error","repository":repository,"error":error.to_string()})
                );
                if failures >= 10 {
                    return Err(error);
                }
            }
        }
        if started.elapsed() >= Duration::from_secs(policy.follow_seconds) {
            println!(
                "{}",
                json!({"event":"follow-timeout","repository":repository})
            );
            return Ok(());
        }
        for _ in 0..policy.interval_seconds {
            if INTERRUPTED.load(Ordering::SeqCst) {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(1));
        }
    }
}

fn spawn_follower(options: &Options, state_dir: &Path, repository: &str) -> Result<()> {
    let slug: String = repository
        .chars()
        .map(|c| if c == '/' { '-' } else { c })
        .collect();
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join(format!("follow-{slug}.log")))?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("land")
        .arg("--follow")
        .arg("--policy")
        .arg(options.policy.as_deref().ok_or("Missing policy")?)
        .arg("--state-dir")
        .arg(state_dir)
        .arg(repository)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    let mut child = detach(command)?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    println!(
        "{}",
        json!({"event":"follower-started","repository":repository})
    );
    Ok(())
}

/// Read-only: each queued entry with the verdict of its exact head.
fn queue_view(policy: &Policy, http: &mut Http, repository: &str) -> Result<Value> {
    use cfrg::land::LandTarget;
    let contexts = policy.contexts(repository);
    let mut target = cfgj::land::Land::new(&policy.endpoint, repository, http)?;
    let mut rows = Vec::new();
    for entry in target.queue()? {
        let statuses = target.statuses(&entry.head)?;
        let verdict = match land::verdict(&statuses, &contexts) {
            State::Success => "success",
            State::Pending => "pending",
            State::Failure => "failure",
        };
        let current = target.contains_tip(&entry)?;
        rows.push(json!({
            "number": entry.number, "branch": entry.branch, "head": entry.head,
            "verdict": verdict, "contains_default_tip": current,
        }));
    }
    Ok(json!({"repository": repository, "queue": rows}))
}
