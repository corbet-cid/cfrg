//! `cfrg gate`: the GitHub write gate (budgets, queue, coalescing, priority lane, kill switch; see `cfrg::gate`).
//! `flush` is a dry run unless `--execute` is given. The write credential is read from the environment variable named by
//! `--token-env` at the last moment and goes to git through the environment, never through arguments or files.
use cfrg::{
    gate::{self, Config, Deny, Entry, Kind, Ledger, Queued},
    process::Runner,
    Environment, Result,
};
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Args)]
pub struct Options {
    /// Gate configuration (JSON): budgets, listed repositories with lane and source, state, pause and kill-switch directories.
    #[arg(long, env = "CFRG_GATE_CONFIG")]
    config: PathBuf,
    /// Environment variable that holds the GitHub write credential (only `flush --execute` and `dispatch --execute` read it).
    #[arg(long, default_value = "CFRG_GITHUB_TOKEN")]
    token_env: String,
    /// Base of the destination URLs; GitHub's, except in tests.
    #[arg(long, default_value = "https://github.com")]
    origin: String,
    /// Base of the GitHub API for `dispatch`; must start with `https://`.
    #[arg(long, default_value = "https://api.github.com")]
    api: String,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Budgets used and left, queue, paused organisations and the kill switch, as one JSON object.
    Status,
    /// Record that repositories have changes (the queue holds names, never commits).
    Enqueue { repositories: Vec<String> },
    /// Create one listed GitHub repository in its organisation, under the creation budgets and gap. A dry run without --execute.
    Create {
        /// `org/repo` listed in the gate configuration.
        repository: String,
        /// Create it private (the Forgejo primary's visibility; visibility is never changed later).
        #[arg(long)]
        private: bool,
        /// Description copied from the Forgejo primary.
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long)]
        execute: bool,
    },
    /// Push the newest state of every queued repository that the budgets allow. A dry run without --execute.
    Flush {
        /// Create a listed destination that does not exist yet (visibility and description from the configuration), under the
        /// creation budgets, and fill it in the same run. Without it a missing destination is only reported.
        #[arg(long)]
        create: bool,
        #[arg(long)]
        execute: bool,
    },
    /// Switch GitHub Actions on (the steady state) or off for one listed repository. A dry run without --execute.
    Actions {
        /// `org/repo` listed in the gate configuration.
        repository: String,
        /// Switch Actions off instead of on.
        #[arg(long)]
        disable: bool,
        #[arg(long)]
        execute: bool,
    },
    /// Start one provider workflow (`workflow_dispatch`) on a build-lane repository. A dry run without --execute.
    Dispatch {
        /// Listed `org/repo` whose lane must be `build`.
        repository: String,
        /// Workflow file name, for example `ccid-remote.yml`.
        workflow: String,
        /// Branch name (no slash, not starting with `.` or `-`).
        #[arg(value_name = "REF")]
        reference: String,
        /// Workflow input as `key=value`; repeat up to ten times.
        #[arg(long = "input")]
        inputs: Vec<String>,
        #[arg(long)]
        execute: bool,
    },
    /// Delete one OLD PRIVATE GitHub repository named in the delete allowlist, if its default-branch head still equals the listed
    /// commit. Refuses a public repository, an unlisted one and a changed head. A dry run (verifying everything, deleting nothing)
    /// without --execute.
    DeleteRepo {
        /// `org/repo` listed in the allowlist.
        repository: String,
        /// Allowlist (JSON): `{"repos": {"org/repo": {"head": "<40-hex commit>"}}}`.
        #[arg(long)]
        allowlist: PathBuf,
        #[arg(long)]
        execute: bool,
    },
}

pub(crate) fn failure(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::other(message.into()).into()
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn run(options: Options) -> Result<()> {
    let config = Config::load(&options.config)?;
    match options.action {
        Action::Status => println!("{}", status(&config)?),
        Action::Enqueue { repositories } => {
            let mut queue = gate::load_queue(&config.state_dir)?;
            for name in &repositories {
                gate::split(name)?;
                if !config.repos.contains_key(name) {
                    return Err(failure(format!(
                        "{name} is not listed in the gate configuration"
                    )));
                }
                gate::enqueue(&mut queue, name, now());
            }
            gate::save_queue(&config.state_dir, &queue)?;
            println!("{}", json!({"queued": queue.len()}));
        }
        Action::Create {
            repository,
            private,
            description,
            execute,
        } => create(
            &config,
            &options.token_env,
            &options.api,
            &repository,
            private,
            &description,
            execute,
        )?,
        Action::Flush { create, execute } => flush(
            &config,
            &options.token_env,
            &options.api,
            &options.origin,
            execute,
            create,
        )?,
        Action::Actions {
            repository,
            disable,
            execute,
        } => actions(
            &config,
            &options.token_env,
            &options.api,
            &repository,
            !disable,
            execute,
        )?,
        Action::Dispatch {
            repository,
            workflow,
            reference,
            inputs,
            execute,
        } => dispatch(
            &config,
            &options.token_env,
            &options.api,
            &repository,
            &workflow,
            &reference,
            &inputs,
            execute,
        )?,
        Action::DeleteRepo {
            repository,
            allowlist,
            execute,
        } => crate::delete::delete_repo(
            &config,
            &options.token_env,
            &options.api,
            &allowlist,
            &repository,
            execute,
        )?,
    }
    Ok(())
}

fn count(entries: &[Entry], since: u64, kind: Kind) -> usize {
    entries
        .iter()
        .filter(|e| e.ts > since && e.kind == kind)
        .count()
}

fn status(config: &Config) -> Result<Value> {
    let now = now();
    let entries = Ledger::new(&config.state_dir).load(now)?;
    let queue = gate::load_queue(&config.state_dir)?;
    let paused: Vec<String> = std::fs::read_dir(&config.paused_dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    let b = &config.budgets;
    let hour = now.saturating_sub(gate::HOUR);
    let day = now.saturating_sub(gate::DAY);
    Ok(json!({
        "kill_switch": config.kill_switch.exists(),
        "hold_tags": config.hold_tags,
        "paused": paused,
        "queue": queue,
        "pushes": {"hour": count(&entries, hour, Kind::Push), "hour_budget": b.pushes_per_hour,
                   "day": count(&entries, day, Kind::Push), "day_budget": b.pushes_per_day},
        "created": {"day": count(&entries, day, Kind::Create), "day_budget": b.new_repos_per_day},
        "actions_runs": {"day": count(&entries, day, Kind::ActionsRun), "day_budget": b.actions_runs_per_day},
    }))
}

fn git_env(extra: &[(&str, String)]) -> Environment {
    let mut environment: Environment = std::env::vars_os().collect();
    environment.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    for (k, v) in extra {
        environment.insert((*k).into(), v.into());
    }
    environment
}

/// Git environment that authenticates HTTPS GitHub operations through an extra header, so the credential never enters argv or a file.
fn auth_environment(token: &str) -> Environment {
    let header = format!(
        "AUTHORIZATION: basic {}",
        base64(format!("x-access-token:{token}").as_bytes())
    );
    git_env(&[
        ("GIT_CONFIG_COUNT", "1".into()),
        (
            "GIT_CONFIG_KEY_0",
            "http.https://github.com/.extraheader".into(),
        ),
        ("GIT_CONFIG_VALUE_0", header),
    ])
}

/// refname to object name, from `for-each-ref` output or `ls-remote` output (peeled tag lines dropped).
fn refs(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let (sha, name) = l.split_once(char::is_whitespace)?;
            let name = name.trim();
            (!name.ends_with("^{}")
                && (name.starts_with("refs/heads/") || name.starts_with("refs/tags/")))
            .then(|| (name.to_string(), sha.to_string()))
        })
        .collect()
}

/// The refs that count for a repository: branches and tags, or branches only while the operator holds tags.
fn counted(mut refs: BTreeMap<String, String>, hold_tags: bool) -> BTreeMap<String, String> {
    if hold_tags {
        refs.retain(|name, _| !name.starts_with("refs/tags/"));
    }
    refs
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |a, (i, b)| a | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn blocked(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    [
        "403",
        "429",
        "402",
        "abuse",
        "rate limit",
        "secondary",
        "too many",
    ]
    .iter()
    .any(|m| e.contains(m))
}

fn workflow_file(value: &str) -> bool {
    !value.contains('/')
        && (value.ends_with(".yml") || value.ends_with(".yaml"))
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn branch_ref(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('.')
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn dispatch_inputs(inputs: &[String]) -> Result<BTreeMap<String, String>> {
    if inputs.len() > 10 {
        return Err(failure("At most 10 workflow inputs are allowed"));
    }
    let mut result = BTreeMap::new();
    for input in inputs {
        let (key, value) = input
            .split_once('=')
            .ok_or_else(|| failure(format!("Workflow input must be key=value: {input}")))?;
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        {
            return Err(failure(format!("Invalid workflow input name: {key}")));
        }
        if value.chars().count() > 200 {
            return Err(failure(format!(
                "Workflow input {key} is longer than 200 characters"
            )));
        }
        if value.chars().any(|c| c.is_control()) {
            return Err(failure(format!(
                "Workflow input {key} contains a control character"
            )));
        }
        if result.insert(key.to_string(), value.to_string()).is_some() {
            return Err(failure(format!("Duplicate workflow input: {key}")));
        }
    }
    Ok(result)
}

pub(crate) fn api_base(api: &str) -> Result<String> {
    let value = api.trim_end_matches('/');
    let authority = value
        .strip_prefix("https://")
        .ok_or_else(|| failure("--api must start with https://"))?;
    if authority.is_empty()
        || authority.starts_with('/')
        || authority
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(failure("--api must be an HTTPS base URL"));
    }
    Ok(value.to_string())
}

pub(crate) struct HttpAnswer {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

fn gate_environment(token: &str) -> Environment {
    let mut environment = Environment::new();
    for key in ["PATH", "TMPDIR", "SSL_CERT_FILE", "SSL_CERT_DIR"] {
        if let Some(value) = std::env::var_os(key) {
            environment.insert(key.into(), value);
        }
    }
    environment.insert("CFRG_GATE_TOKEN".into(), token.into());
    environment
}

/// One GitHub API call. The credential enters curl through `--variable`/`--expand-header`, never argv,
/// files or logs; redirects, ambient proxies and response-body diagnostics stay off.
fn api_request(
    api: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<HttpAnswer> {
    let runner = Runner::new(
        std::env::current_dir()?,
        gate_environment(token),
        Duration::from_secs(45),
    )?
    .with_stderr_events()
    .without_child_stderr();
    let output = tempfile::NamedTempFile::new()?;
    let mut input = tempfile::NamedTempFile::new()?;
    let mut args: Vec<String> = [
        "curl",
        "--disable",
        "--silent",
        "--globoff",
        "--connect-timeout",
        "10",
        "--max-time",
        "30",
        "--max-filesize",
        "4194304",
        "--proto",
        "=https",
        "--variable",
        "%CFRG_GATE_TOKEN",
        "--expand-header",
        "Authorization: token {{CFRG_GATE_TOKEN}}",
        "--header",
        "Accept: application/json",
        "--output",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    args.push(output.path().to_string_lossy().into_owned());
    args.extend([
        "--write-out".into(),
        "%{http_code}".into(),
        "--request".into(),
        method.into(),
        "--url".into(),
        format!("{api}{path}"),
    ]);
    if let Some(body) = body {
        serde_json::to_writer(&mut input, body)?;
        input.flush()?;
        args.extend([
            "--header".into(),
            "Content-Type: application/json".into(),
            "--data-binary".into(),
            format!("@{}", input.path().display()),
        ]);
    }
    let status: u16 = runner
        .run(&args, true)?
        .trim()
        .parse()
        .map_err(|_| failure("Unknown HTTP outcome"))?;
    if status == 0 {
        return Err(failure("Unknown HTTP outcome"));
    }
    Ok(HttpAnswer {
        status,
        body: std::fs::read(output.path())?,
    })
}

pub(crate) fn github(
    config: &Config,
    org: &str,
    api: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<HttpAnswer> {
    match api_request(api, token, method, path, body) {
        Ok(answer) => {
            if blocked(&format!("HTTP {}", answer.status)) {
                // The first sign of throttling or abuse pauses the whole organisation until Julian resumes it.
                std::fs::create_dir_all(config.paused_dir.join(org))?;
                return Err(failure(format!(
                    "{org} paused after HTTP {}",
                    answer.status
                )));
            }
            Ok(answer)
        }
        Err(error) => {
            if blocked(&error.to_string()) {
                std::fs::create_dir_all(config.paused_dir.join(org))?;
                return Err(failure(format!("{org} paused after a throttling answer")));
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn create(
    config: &Config,
    token_env: &str,
    api: &str,
    name: &str,
    private: bool,
    description: &str,
    execute: bool,
) -> Result<()> {
    let (org, repo) = gate::split(name)?;
    if !config.repos.contains_key(name) {
        return Err(failure(format!(
            "{name} is not listed in the gate configuration"
        )));
    }
    if description.chars().count() > 350 || description.chars().any(char::is_control) {
        return Err(failure(
            "Description must be at most 350 characters without control characters",
        ));
    }
    let api = api_base(api)?;
    let now = now();
    let ledger = Ledger::new(&config.state_dir);
    let entries = ledger.load(now)?;
    let world = gate::World {
        now,
        ledger: &entries,
        kill_switch_set: config.kill_switch.exists(),
        org_paused: config.paused_dir.join(org).exists(),
    };
    if let Err(deny) = gate::decide(
        &config.budgets,
        &world,
        gate::Lane::Bulk,
        org,
        repo,
        Kind::Create,
    ) {
        println!("{}", json!({"repo": name, "verdict": deny.text()}));
        return Err(failure(deny.text()));
    }
    if !execute {
        println!(
            "{}",
            json!({"repo": name, "verdict": "would create", "private": private})
        );
        return Ok(());
    }
    let token = std::env::var(token_env)
        .map_err(|_| failure(format!("{token_env} is not set; nothing was created")))?;
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(failure("Invalid credential format"));
    }
    // Read first: never spend a creation on an existing repository (the POST would answer 422).
    let existing = github(
        config,
        org,
        &api,
        &token,
        "GET",
        &format!("/repos/{org}/{repo}"),
        None,
    )?;
    if existing.status == 200 {
        return Err(failure(format!(
            "{name} already exists; visibility is never changed"
        )));
    }
    if existing.status != 404 {
        return Err(failure(format!(
            "GitHub API HTTP {} reading the destination",
            existing.status
        )));
    }
    // Count first: a crash between this line and the call can only over-count.
    ledger.append(&Entry {
        ts: now,
        kind: Kind::Create,
        org: org.into(),
        repo: repo.into(),
        lane: gate::Lane::Bulk,
    })?;
    let created = create_repository(config, &api, &token, org, repo, private, description)?;
    println!(
        "{}",
        json!({"repo": name, "verdict": "created",
               "id": created.get("id").cloned().unwrap_or(Value::Null),
               "private": created.get("private").cloned().unwrap_or(Value::Null)})
    );
    Ok(())
}

/// One `POST /orgs/<org>/repos` after a read that refuses an existing destination. No ledger work: the caller has already counted the creation.
fn create_repository(
    config: &Config,
    api: &str,
    token: &str,
    org: &str,
    repo: &str,
    private: bool,
    description: &str,
) -> Result<Value> {
    // Read first: never spend a creation on an existing repository (the POST would answer 422).
    let existing = github(
        config,
        org,
        api,
        token,
        "GET",
        &format!("/repos/{org}/{repo}"),
        None,
    )?;
    if existing.status == 200 {
        return Err(failure(format!(
            "{org}/{repo} already exists; visibility is never changed"
        )));
    }
    if existing.status != 404 {
        return Err(failure(format!(
            "GitHub API HTTP {} reading the destination",
            existing.status
        )));
    }
    let body = json!({
        "name": repo,
        "private": private,
        "description": description,
        "has_issues": false,
        "has_projects": false,
        "has_wiki": false,
        "auto_init": false,
    });
    let answer = github(
        config,
        org,
        api,
        token,
        "POST",
        &format!("/orgs/{org}/repos"),
        Some(&body),
    )?;
    if answer.status != 201 {
        return Err(failure(format!(
            "GitHub API HTTP {} creating the repository",
            answer.status
        )));
    }
    let created: Value = serde_json::from_slice(&answer.body)
        .map_err(|_| failure("Malformed GitHub repository response"))?;
    Ok(created)
}

/// Switch GitHub Actions at repository level. A fill disables them for the first push, because a new
/// destination receives every historical branch and tag: those commits still carry the old workflow
/// files, and a tag push would start release runs. Steady state stays enabled (Julian: regulation lives
/// in the code, not in a switched-off Actions tab).
fn actions_enabled(config: &Config, org: &str, api: &str, token: &str, repo: &str) -> Result<bool> {
    let answer = github(
        config,
        org,
        api,
        token,
        "GET",
        &format!("/repos/{org}/{repo}/actions/permissions"),
        None,
    )?;
    if answer.status != 200 {
        return Err(failure(format!(
            "GitHub API HTTP {} reading Actions permissions",
            answer.status
        )));
    }
    let value: Value = serde_json::from_slice(&answer.body)
        .map_err(|_| failure("Malformed GitHub Actions permissions response"))?;
    value
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| failure("GitHub Actions permissions response lacks enabled"))
}

/// Switch Actions off and prove it: the setting is not strongly consistent, so a push sent in the same second still starts runs
/// from the historical workflow files. The first push of a fresh destination therefore waits for the NEXT flush, and this
/// function refuses the push when the switch cannot be seen as off.
fn ensure_actions_disabled(
    config: &Config,
    org: &str,
    api: &str,
    token: &str,
    repo: &str,
) -> Result<()> {
    for attempt in 0..5 {
        if !actions_enabled(config, org, api, token, repo)? {
            return Ok(());
        }
        set_actions_enabled(config, org, api, token, repo, false)?;
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }
    Err(failure(format!(
        "Actions could not be switched off at {org}/{repo}; nothing was pushed"
    )))
}

fn set_actions_enabled(
    config: &Config,
    org: &str,
    api: &str,
    token: &str,
    repo: &str,
    enabled: bool,
) -> Result<()> {
    let path = format!("/repos/{org}/{repo}/actions/permissions");
    let answer = github(
        config,
        org,
        api,
        token,
        "PUT",
        &path,
        Some(&json!({"enabled": enabled})),
    )?;
    if !matches!(answer.status, 200 | 204) {
        return Err(failure(format!(
            "GitHub API HTTP {} switching Actions {}",
            answer.status,
            if enabled { "on" } else { "off" }
        )));
    }
    Ok(())
}

/// What a fresh destination shows right after its first push. A plain push must start no workflow run. Dependabot's own
/// version-update jobs are not workflow runs; they are accepted only while they open no pull request (the GitHub copies are
/// push mirrors, so a Dependabot PR there can never be merged back and is pure bot traffic). Any other run, and any bot pull
/// request, is a STOP signal.
#[derive(Default, Debug, PartialEq, Eq)]
struct InitialScan {
    offending_runs: u64,
    dependabot_runs: u64,
    dependabot_running: u64,
    bot_pulls: u64,
}

fn initial_scan(
    config: &Config,
    org: &str,
    api: &str,
    token: &str,
    repo: &str,
) -> Result<InitialScan> {
    let mut scan = InitialScan::default();
    let answer = github(
        config,
        org,
        api,
        token,
        "GET",
        &format!("/repos/{org}/{repo}/actions/runs?per_page=100"),
        None,
    )?;
    if answer.status != 200 {
        return Err(failure(format!(
            "GitHub API HTTP {} reading Actions runs",
            answer.status
        )));
    }
    let value: Value = serde_json::from_slice(&answer.body)
        .map_err(|_| failure("Malformed GitHub Actions runs response"))?;
    for run in value
        .get("workflow_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let event = run.get("event").and_then(Value::as_str).unwrap_or_default();
        let actor = run
            .get("actor")
            .and_then(|a| a.get("login"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if event == "dynamic" && actor == "dependabot[bot]" {
            scan.dependabot_runs += 1;
            if run.get("status").and_then(Value::as_str) != Some("completed") {
                scan.dependabot_running += 1;
            }
        } else {
            scan.offending_runs += 1;
        }
    }
    let answer = github(
        config,
        org,
        api,
        token,
        "GET",
        &format!("/repos/{org}/{repo}/pulls?state=all&per_page=100"),
        None,
    )?;
    if answer.status != 200 {
        return Err(failure(format!(
            "GitHub API HTTP {} reading pull requests",
            answer.status
        )));
    }
    let pulls: Value = serde_json::from_slice(&answer.body)
        .map_err(|_| failure("Malformed GitHub pull request response"))?;
    for pull in pulls.as_array().into_iter().flatten() {
        let user = pull.get("user");
        let kind = user
            .and_then(|u| u.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let login = user
            .and_then(|u| u.get("login"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "Bot" || login.ends_with("[bot]") {
            scan.bot_pulls += 1;
        }
    }
    Ok(scan)
}

/// Switch Actions for one listed repository through the gate: the steady state is enabled (Julian: regulation lives in the
/// code), a first push runs with them off. Refuses while the kill switch is set or the organisation is paused.
fn actions(
    config: &Config,
    token_env: &str,
    api: &str,
    name: &str,
    enabled: bool,
    execute: bool,
) -> Result<()> {
    let (org, repo) = gate::split(name)?;
    if !config.repos.contains_key(name) {
        return Err(failure(format!(
            "{name} is not listed in the gate configuration"
        )));
    }
    if config.kill_switch.exists() {
        return Err(failure("kill switch is set"));
    }
    if config.paused_dir.join(org).exists() {
        return Err(failure(format!("{org} is paused")));
    }
    if !execute {
        println!(
            "{}",
            json!({"repo": name, "verdict": if enabled { "would enable Actions" } else { "would disable Actions" }})
        );
        return Ok(());
    }
    let token = std::env::var(token_env).map_err(|_| {
        failure(format!(
            "the credential environment variable {token_env} is not set"
        ))
    })?;
    if token.is_empty() {
        return Err(failure(format!(
            "the credential environment variable {token_env} is empty"
        )));
    }
    let api = api_base(api)?;
    set_actions_enabled(config, org, &api, &token, repo, enabled)?;
    if actions_enabled(config, org, &api, &token, repo)? != enabled {
        return Err(failure(format!(
            "Actions could not be switched {} at {org}/{repo}",
            if enabled { "on" } else { "off" }
        )));
    }
    println!(
        "{}",
        json!({"repo": name, "verdict": if enabled { "Actions enabled" } else { "Actions disabled" }})
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    config: &Config,
    token_env: &str,
    api: &str,
    name: &str,
    workflow: &str,
    reference: &str,
    inputs: &[String],
    execute: bool,
) -> Result<()> {
    let (org, repo) = gate::split(name)?;
    let Some(entry) = config.repos.get(name) else {
        return Err(failure(format!(
            "{name} is not listed in the gate configuration"
        )));
    };
    if entry.lane != gate::Lane::Build {
        return Err(failure(format!(
            "only build-lane repositories run GitHub Actions: {name}"
        )));
    }
    if !workflow_file(workflow) {
        return Err(failure(format!("Invalid workflow file name: {workflow}")));
    }
    if !branch_ref(reference) {
        return Err(failure(format!("Invalid ref: {reference}")));
    }
    let inputs = dispatch_inputs(inputs)?;
    let api = api_base(api)?;
    let now = now();
    let ledger = Ledger::new(&config.state_dir);
    let entries = ledger.load(now)?;
    let world = gate::World {
        now,
        ledger: &entries,
        kill_switch_set: config.kill_switch.exists(),
        org_paused: config.paused_dir.join(org).exists(),
    };
    if let Err(deny) = gate::decide(
        &config.budgets,
        &world,
        gate::Lane::Build,
        org,
        repo,
        Kind::ActionsRun,
    ) {
        println!("{}", json!({"repo": name, "verdict": deny.text()}));
        return Err(failure(deny.text()));
    }
    if !execute {
        println!(
            "{}",
            json!({"repo": name, "verdict": "would dispatch", "workflow": workflow, "ref": reference})
        );
        return Ok(());
    }
    let token = std::env::var(token_env)
        .map_err(|_| failure(format!("{token_env} is not set; nothing was dispatched")))?;
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(failure("Invalid credential format"));
    }
    // Count first: a crash between this line and the calls can only over-count.
    ledger.append(&Entry {
        ts: now,
        kind: Kind::ActionsRun,
        org: org.into(),
        repo: repo.into(),
        lane: gate::Lane::Build,
    })?;

    let permissions_path = format!("/repos/{org}/{repo}/actions/permissions");
    let permissions = github(config, org, &api, &token, "GET", &permissions_path, None)?;
    if permissions.status != 200 {
        return Err(failure(format!(
            "GitHub API HTTP {} reading Actions permissions",
            permissions.status
        )));
    }
    let permissions: Value = serde_json::from_slice(&permissions.body)
        .map_err(|_| failure("Malformed GitHub Actions permissions response"))?;
    let enabled = permissions
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| failure("GitHub Actions permissions response lacks enabled"))?;
    if !enabled {
        let enabled_answer = github(
            config,
            org,
            &api,
            &token,
            "PUT",
            &permissions_path,
            Some(&json!({"enabled": true, "allowed_actions": "selected"})),
        )?;
        if enabled_answer.status != 204 {
            return Err(failure(format!(
                "GitHub API HTTP {} enabling Actions",
                enabled_answer.status
            )));
        }
        let selected_path = format!("/repos/{org}/{repo}/actions/permissions/selected-actions");
        let selected = github(
            config,
            org,
            &api,
            &token,
            "PUT",
            &selected_path,
            Some(&json!({
                "github_owned_allowed": true,
                "verified_allowed": false,
                "patterns_allowed": []
            })),
        )?;
        if selected.status != 204 {
            return Err(failure(format!(
                "GitHub API HTTP {} selecting Actions",
                selected.status
            )));
        }
        println!("{}", json!({"repo": name, "verdict": "actions enabled"}));
    }

    let dispatch_path = format!("/repos/{org}/{repo}/actions/workflows/{workflow}/dispatches");
    let answer = github(
        config,
        org,
        &api,
        &token,
        "POST",
        &dispatch_path,
        Some(&json!({"ref": reference, "inputs": inputs})),
    )?;
    if !matches!(answer.status, 200 | 204) {
        return Err(failure(format!(
            "GitHub API HTTP {} dispatching workflow",
            answer.status
        )));
    }
    println!(
        "{}",
        json!({"repo": name, "verdict": "dispatched", "workflow": workflow, "ref": reference})
    );
    Ok(())
}

fn flush(
    config: &Config,
    token_env: &str,
    api: &str,
    origin: &str,
    execute: bool,
    create_missing: bool,
) -> Result<()> {
    let api = api_base(api)?;
    let ledger = Ledger::new(&config.state_dir);
    let mut entries = ledger.load(now())?;
    let mut queue = gate::load_queue(&config.state_dir)?;
    let order = gate::order(config, &queue);
    // An executing flush reads the credential once: the destination read (ls-remote), every creation and every push use it, so a
    // fill never falls back to anonymous requests (STOP 7 of ds-ghfill: no anonymous probing of missing destinations).
    let token = if execute {
        let value = std::env::var(token_env)
            .map_err(|_| failure(format!("{token_env} is not set; nothing was pushed")))?;
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(failure("Invalid credential format"));
        }
        Some(value)
    } else {
        None
    };
    let read_env = match &token {
        Some(token) => auth_environment(token),
        None => git_env(&[]),
    };
    let mut report = Vec::new();
    for Queued { repo: name, .. } in order {
        let (org, repo) = gate::split(&name)?;
        let Some(entry) = config.repos.get(&name) else {
            report.push(json!({"repo": name, "verdict": Deny::UnknownRepo.text()}));
            continue;
        };
        let url = format!("{origin}/{name}.git");
        if !entry.source.exists() {
            // The srv git copy follows the primary's renames; a fill may list a repository before its copy arrives.
            report.push(json!({"repo": name, "verdict": "source missing on this host (git copy not there yet)"}));
            continue;
        }
        let local = Runner::new(entry.source.clone(), git_env(&[]), Duration::from_secs(120))?
            .with_stderr_events()
            .run(
                &[
                    "git".into(),
                    "for-each-ref".into(),
                    "--format=%(objectname) %(refname)".into(),
                    "refs/heads".into(),
                    "refs/tags".into(),
                ],
                true,
            )?;
        let local = counted(refs(&local), config.hold_tags);
        if local.is_empty() {
            report.push(json!({"repo": name, "verdict": "source has no branches yet"}));
            continue;
        }
        let remote = Runner::new(
            entry.source.clone(),
            read_env.clone(),
            Duration::from_secs(120),
        )?
        .with_stderr_events()
        .without_child_stderr()
        .run(
            &[
                "git".into(),
                "ls-remote".into(),
                url.clone(),
                "refs/heads/*".into(),
                "refs/tags/*".into(),
            ],
            true,
        )
        .ok()
        .map(|t| counted(refs(&t), config.hold_tags));
        let mut remote = remote;
        let mut created = false;
        if remote.is_none() {
            // The destination does not exist. Creation is its own budgeted mutation; with --create the fill does it here.
            if !create_missing {
                let now = now();
                let world = gate::World {
                    now,
                    ledger: &entries,
                    kill_switch_set: config.kill_switch.exists(),
                    org_paused: config.paused_dir.join(org).exists(),
                };
                let verdict =
                    gate::decide(&config.budgets, &world, entry.lane, org, repo, Kind::Create);
                report.push(json!({"repo": name, "verdict": "destination missing: create it first (cfrg gate create)",
                                   "creation_allowed": verdict.is_ok()}));
                continue;
            }
            let now = now();
            let world = gate::World {
                now,
                ledger: &entries,
                kill_switch_set: config.kill_switch.exists(),
                org_paused: config.paused_dir.join(org).exists(),
            };
            if let Err(deny) = gate::decide(
                &config.budgets,
                &world,
                gate::Lane::Bulk,
                org,
                repo,
                Kind::Create,
            ) {
                report.push(
                    json!({"repo": name, "verdict": format!("creation held: {}", deny.text())}),
                );
                if deny == Deny::KillSwitch {
                    break;
                }
                continue;
            }
            if !execute {
                report.push(json!({"repo": name, "verdict": "would create and fill", "private": entry.private}));
                continue;
            }
            let token = token.as_deref().unwrap_or_default();
            // Count first: a crash between this line and the POST can only over-count.
            let creation = Entry {
                ts: now,
                kind: Kind::Create,
                org: org.into(),
                repo: repo.into(),
                lane: gate::Lane::Bulk,
            };
            ledger.append(&creation)?;
            entries.push(creation);
            let created_repo = create_repository(
                config,
                &api,
                token,
                org,
                repo,
                entry.private,
                &entry.description,
            )?;
            created = true;
            remote = Some(BTreeMap::new());
            report.push(json!({"repo": name, "verdict": "created",
                               "id": created_repo.get("id").cloned().unwrap_or(Value::Null),
                               "private": created_repo.get("private").cloned().unwrap_or(Value::Null)}));
            if config.quiet_fill {
                // Do not push in the same run: the Actions switch must be seen off first, and GitHub propagates it asynchronously.
                // The repository stays queued; the next flush pushes it with Actions off and enables them afterwards.
                ensure_actions_disabled(config, org, &api, token, repo)?;
                report.push(json!({"repo": name, "verdict": "Actions off and verified; the first push waits for the next flush"}));
                continue;
            }
        }
        if remote.as_ref() == Some(&local) {
            queue.retain(|q| q.repo != name);
            report.push(json!({"repo": name, "verdict": "in sync, dequeued"}));
            continue;
        }
        let now = now();
        let world = gate::World {
            now,
            ledger: &entries,
            kill_switch_set: config.kill_switch.exists(),
            org_paused: config.paused_dir.join(org).exists(),
        };
        if let Err(deny) = gate::decide(&config.budgets, &world, entry.lane, org, repo, Kind::Push)
        {
            report.push(json!({"repo": name, "verdict": format!("held: {}", deny.text())}));
            if deny == Deny::KillSwitch {
                break;
            }
            continue;
        }
        let changed = local
            .iter()
            .filter(|(k, v)| remote.as_ref().and_then(|r| r.get(*k)) != Some(*v))
            .count();
        if !execute {
            report.push(json!({"repo": name, "verdict": "would push", "lane": entry.lane, "refs_changed": changed}));
            continue;
        }
        let token = token
            .as_deref()
            .ok_or_else(|| failure("an executing flush needs the credential"))?;
        // Count first: a crash between this line and the push can only over-count.
        let mut mutations = vec![Entry {
            ts: now,
            kind: Kind::Push,
            org: org.into(),
            repo: repo.into(),
            lane: entry.lane,
        }];
        if entry.lane == gate::Lane::Build {
            mutations.push(Entry {
                ts: now,
                kind: Kind::ActionsRun,
                org: org.into(),
                repo: repo.into(),
                lane: entry.lane,
            });
        }
        for m in &mutations {
            ledger.append(m)?;
            entries.push(m.clone());
        }
        // A fresh destination receives every historical branch and tag. Those commits still carry the old workflow files, so a tag
        // push would start release runs and branch pushes could start branch CI. The first push runs with Actions verifiably off;
        // they are enabled again afterwards and `actions/runs` must stay empty. The steady state stays enabled.
        let initial =
            config.quiet_fill && (created || remote.as_ref().is_some_and(|r| r.is_empty()));
        if initial {
            ensure_actions_disabled(config, org, &api, token, repo)?;
        }
        let environment = auth_environment(token);
        let mut push_args: Vec<String> = vec![
            "git".into(),
            "push".into(),
            "--quiet".into(),
            url.clone(),
            "refs/heads/*:refs/heads/*".into(),
        ];
        if !config.hold_tags {
            push_args.push("refs/tags/*:refs/tags/*".into());
        }
        let pushed = Runner::new(
            entry.source.clone(),
            environment.clone(),
            Duration::from_secs(900),
        )?
        .with_stderr_events()
        .run(&push_args, true);
        // LFS objects do not travel with the ref push: without them the destination holds pointers only.
        let lfs = if pushed.is_ok() {
            let listed = Runner::new(
                entry.source.clone(),
                environment.clone(),
                Duration::from_secs(300),
            )?
            .with_stderr_events()
            .without_child_stderr()
            .run(
                &[
                    "git".into(),
                    "lfs".into(),
                    "ls-files".into(),
                    "--all".into(),
                    "--long".into(),
                ],
                true,
            )?;
            let objects: Vec<String> = listed
                .lines()
                .filter_map(|line| line.split_whitespace().next().map(String::from))
                .filter(|oid| oid.len() == 64 && oid.bytes().all(|b| b.is_ascii_hexdigit()))
                .collect();
            if objects.is_empty() {
                Ok(())
            } else {
                let mut args: Vec<String> = [
                    "git".into(),
                    "lfs".into(),
                    "push".into(),
                    "--object-id".into(),
                    url.clone(),
                ]
                .into_iter()
                .collect();
                args.extend(objects);
                Runner::new(
                    entry.source.clone(),
                    environment.clone(),
                    Duration::from_secs(900),
                )?
                .with_stderr_events()
                .run(&args, true)
                .map(|_| ())
            }
        } else {
            Ok(())
        };
        if initial {
            // Let the run index catch up before the verdict: a run created by the push appears a few seconds late (the cmeet incident of
            // 2026-11-08 03:45Z: one run started because the disable had not propagated; this check now waits and the push itself only
            // happens a flush after the verified disable). Dependabot's own version-update jobs are tolerated only when they close
            // without opening a pull request; any other run, and any bot pull request, pauses the organisation.
            let mut scan = InitialScan::default();
            for _ in 0..3 {
                std::thread::sleep(std::time::Duration::from_secs(10));
                scan = initial_scan(config, org, &api, token, repo)?;
                if scan.offending_runs > 0 || scan.bot_pulls > 0 || scan.dependabot_running > 0 {
                    break;
                }
            }
            if scan.offending_runs == 0 && scan.bot_pulls == 0 && scan.dependabot_running > 0 {
                for _ in 0..12 {
                    std::thread::sleep(std::time::Duration::from_secs(10));
                    scan = initial_scan(config, org, &api, token, repo)?;
                    if scan.offending_runs > 0 || scan.bot_pulls > 0 || scan.dependabot_running == 0
                    {
                        break;
                    }
                }
            }
            if scan.offending_runs > 0 || scan.bot_pulls > 0 || scan.dependabot_running > 0 {
                std::fs::create_dir_all(config.paused_dir.join(org))?;
                gate::save_queue(&config.state_dir, &queue)?;
                let reason = format!(
                    "{org} paused: {} unexpected workflow runs, {} bot pull requests and {} Dependabot runs still open after the first push to {name}",
                    scan.offending_runs, scan.bot_pulls, scan.dependabot_running
                );
                println!("{}", json!({"flush": report, "stopped": reason}));
                return Err(failure(reason));
            }
            report.push(json!({"repo": name, "verdict": "first push clean",
                               "workflow_runs": scan.offending_runs,
                               "dependabot_runs": scan.dependabot_runs,
                               "bot_pulls": scan.bot_pulls}));
            // Always restore the steady state, even after a failed push: a disabled Actions tab is not the regulation.
            set_actions_enabled(config, org, &api, token, repo, true)?;
            if !actions_enabled(config, org, &api, token, repo)? {
                return Err(failure(format!(
                    "Actions could not be switched back on at {org}/{repo}"
                )));
            }
        }
        match (pushed, lfs) {
            (Ok(_), Ok(())) => {
                queue.retain(|q| q.repo != name);
                report.push(json!({"repo": name, "verdict": "pushed", "lane": entry.lane, "refs_changed": changed}));
            }
            (pushed, lfs) => {
                let error = pushed
                    .err()
                    .or_else(|| lfs.err())
                    .map(|e| e.to_string())
                    .unwrap_or_default();
                let text = error;
                report.push(json!({"repo": name, "verdict": "push failed"}));
                if blocked(&text) {
                    std::fs::create_dir_all(config.paused_dir.join(org))?;
                    gate::save_queue(&config.state_dir, &queue)?;
                    println!(
                        "{}",
                        json!({"flush": report, "stopped": format!("{org} paused after: {}", text.lines().next().unwrap_or(""))})
                    );
                    return Err(failure(format!("{org} paused")));
                }
            }
        }
    }
    gate::save_queue(&config.state_dir, &queue)?;
    println!("{}", json!({"flush": report, "executed": execute}));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn refs_keep_heads_and_tags_and_drop_peeled_lines() {
        let text =
            "aaa refs/heads/main\nbbb refs/tags/v1\nccc refs/tags/v1^{}\nddd refs/pull/1/head\n";
        let map = refs(text);
        assert_eq!(map.len(), 2);
        assert_eq!(map["refs/heads/main"], "aaa");
    }

    #[test]
    fn throttling_answers_stop_the_lane() {
        assert!(blocked("remote: Permission denied ... 403"));
        assert!(blocked("HTTP 429 Too Many Requests"));
        assert!(blocked("You have triggered an abuse detection mechanism"));
        assert!(!blocked("non-fast-forward"));
    }
}
