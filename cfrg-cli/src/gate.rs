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
    /// Push the newest state of every queued repository that the budgets allow. A dry run without --execute.
    Flush {
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
        Action::Flush { execute } => flush(&config, &options.token_env, &options.origin, execute)?,
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

fn flush(config: &Config, token_env: &str, origin: &str, execute: bool) -> Result<()> {
    let ledger = Ledger::new(&config.state_dir);
    let mut entries = ledger.load(now())?;
    let mut queue = gate::load_queue(&config.state_dir)?;
    let order = gate::order(config, &queue);
    let mut report = Vec::new();
    for Queued { repo: name, .. } in order {
        let (org, repo) = gate::split(&name)?;
        let Some(entry) = config.repos.get(&name) else {
            report.push(json!({"repo": name, "verdict": Deny::UnknownRepo.text()}));
            continue;
        };
        let url = format!("{origin}/{name}.git");
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
        let local = refs(&local);
        if local.is_empty() {
            report.push(json!({"repo": name, "verdict": "source has no branches yet"}));
            continue;
        }
        let remote = Runner::new(entry.source.clone(), git_env(&[]), Duration::from_secs(120))?
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
            .map(|t| refs(&t));
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
        if remote.is_none() {
            // The destination does not exist: creation is its own budgeted mutation, done by `cfrg native` (the provider's ensure), not here.
            let verdict =
                gate::decide(&config.budgets, &world, entry.lane, org, repo, Kind::Create);
            report.push(json!({"repo": name, "verdict": "destination missing: create it first (cfrg native ensure)",
                               "creation_allowed": verdict.is_ok()}));
            continue;
        }
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
        let Ok(token) = std::env::var(token_env) else {
            return Err(failure(format!(
                "{token_env} is not set; nothing was pushed"
            )));
        };
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
        let header = format!(
            "AUTHORIZATION: basic {}",
            base64(format!("x-access-token:{token}").as_bytes())
        );
        let environment = git_env(&[
            ("GIT_CONFIG_COUNT", "1".into()),
            (
                "GIT_CONFIG_KEY_0",
                "http.https://github.com/.extraheader".into(),
            ),
            ("GIT_CONFIG_VALUE_0", header),
        ]);
        let pushed = Runner::new(entry.source.clone(), environment, Duration::from_secs(900))?
            .with_stderr_events()
            .run(
                &[
                    "git".into(),
                    "push".into(),
                    "--quiet".into(),
                    url,
                    "refs/heads/*:refs/heads/*".into(),
                    "refs/tags/*:refs/tags/*".into(),
                ],
                true,
            );
        match pushed {
            Ok(_) => {
                queue.retain(|q| q.repo != name);
                report.push(json!({"repo": name, "verdict": "pushed", "lane": entry.lane, "refs_changed": changed}));
            }
            Err(error) => {
                let text = error.to_string();
                report.push(json!({"repo": name, "verdict": "push failed"}));
                if blocked(&text) {
                    // The first sign of throttling or abuse handling stops the whole organisation until Julian resumes it.
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
