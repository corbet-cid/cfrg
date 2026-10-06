//! The `cfrg` command line: placement, sync, status, bridge, collect, access.

#![forbid(unsafe_code)]

use cfrg::process::Runner;
use cfrg::{
    access::{GrantTable, RoleMaps},
    bridge, collect,
    model::Forge,
    placement::{execution_decision, Policy, RunState},
    status::{self, StatusTargets},
    Environment, Result, INTERRUPTED,
};
use clap::{Parser, Subcommand};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    process::ExitCode,
    sync::atomic::Ordering,
    time::Duration,
};

mod resolve;

#[derive(Parser)]
#[command(
    about = "Forge placement, reconciliation, status, bridge, evidence collection and access",
    version
)]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Validate all placements, costs, primary CI/forge relationships and rules.
    Validate {
        #[arg(long)]
        policy: PathBuf,
    },
    /// Ordered clone sources and explicit execution fallback policy; no API writes.
    Plan {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        repository: String,
    },
    /// Evaluate exact-request observations supplied by a provider adapter.
    Decide {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        repository: String,
        #[arg(long)]
        observations: PathBuf,
        #[arg(long = "require", required = true)]
        capabilities: Vec<String>,
    },
    /// Fetch one exact commit, trying only declared mirrors, into a NEW directory.
    /// Submodules and LFS payloads require separate exact source staging.
    Clone {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        repository: String,
        #[arg(long)]
        commit: String,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    },
    /// Reconcile declared secondaries from the primary; writes require --apply.
    Sync {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        repository: String,
        #[arg(
            long = "ref",
            required_unless_present = "all_refs",
            conflicts_with = "all_refs"
        )]
        refs: Vec<String>,
        #[arg(long)]
        all_refs: bool,
        #[arg(long = "to", visible_alias = "destination", required = true)]
        destinations: Vec<String>,
        #[arg(long)]
        apply: bool,
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    },
    /// Mirror repository access across forges from a three-way merge.
    /// `access plan` never writes; `access apply` advances the baseline state
    /// and, once a live transport lands, executes the planned API calls.
    Access {
        #[command(subcommand)]
        action: AccessAction,
    },
    Status(cfrg::status::Options),
    Bridge(cfrg::bridge::Options),
    Collect(cfrg::collect::Options),
    SourceRevision,
    /// Resolve declared stores for one request (pure clmr selection over
    /// bounded probes); prints response JSON or rendered git config.
    Resolve {
        #[arg(long)]
        request: PathBuf,
        #[arg(long, default_value = "json")]
        emit: String,
    },
    /// Git credential helper: answers `get` from per-job env only.
    CredentialHelper {
        #[arg(long)]
        env: String,
        #[arg(long)]
        expect_origin: String,
        #[arg(long)]
        username: Option<String>,
        /// Git appends the action (`get`, `store`, `erase`); only `get` answers.
        action: String,
    },
}

/// Offline access reconciliation. Observations are collector-supplied JSON
/// snapshots, one per forge; no provider API calls are made here.
#[derive(Subcommand)]
enum AccessAction {
    /// Print the exact intended API calls without writing anything.
    Plan {
        #[arg(long)]
        identities: PathBuf,
        #[arg(long)]
        baseline: PathBuf,
        /// One snapshot per forge as `forge=path` (github, forgejo, gitlab,
        /// bitbucket). Repeat for each observed forge.
        #[arg(long = "observed", required = true)]
        observed: Vec<String>,
        /// Stable person id whose own access is never touched.
        #[arg(long)]
        operator: Option<String>,
        /// Forges that are read but never written. Defaults to github.
        #[arg(long = "frozen", default_value = "github")]
        frozen: Vec<String>,
    },
    /// Advance the baseline state. Without --initialize this requires a
    /// present baseline and a converged plan; forge writes run through the
    /// transport, which is unwired in this draft and refuses every call.
    Apply {
        #[arg(long)]
        identities: PathBuf,
        #[arg(long)]
        baseline: PathBuf,
        #[arg(long = "observed", required = true)]
        observed: Vec<String>,
        #[arg(long)]
        operator: Option<String>,
        #[arg(long = "frozen", default_value = "github")]
        frozen: Vec<String>,
        /// Bootstrap the baseline from current observations without planning
        /// any forge writes.
        #[arg(long)]
        initialize: bool,
        /// Where to write the next baseline. Defaults to --baseline.
        #[arg(long)]
        state_out: Option<PathBuf>,
        #[arg(long, default_value_t = 500)]
        pace_ms: u64,
    },
}

fn read(path: PathBuf) -> Result<Policy> {
    let policy: Policy = serde_json::from_slice(&fs::read(path)?)?;
    policy.validate()?;
    Ok(policy)
}

/// Load one `--observed forge=path` snapshot argument.
fn read_observed(observed: &[String]) -> Result<Vec<cfrg::access::ObservedFile>> {
    let mut files = Vec::new();
    for item in observed {
        let (forge, path) = item
            .split_once('=')
            .ok_or_else(|| "Observed snapshots require forge=path".to_string())?;
        let file: cfrg::access::ObservedFile = serde_json::from_slice(
            &fs::read(path)
                .map_err(|error| format!("Cannot read {forge} snapshot {path}: {error}"))?,
        )?;
        if file.forge != cfrg::model::Forge::parse(forge)? {
            return Err(format!("Snapshot {path} declares a different forge").into());
        }
        files.push(file);
    }
    Ok(files)
}

fn access_inputs(
    identities: &PathBuf,
    baseline: &PathBuf,
    observed: &[String],
    operator: &Option<String>,
    frozen: &[String],
) -> Result<(
    cfrg::access::IdentityMap,
    Option<cfrg::access::Baseline>,
    Vec<cfrg::access::ObservedFile>,
    cfrg::access::PlanOptions,
)> {
    let text = fs::read_to_string(identities)?;
    let maps: cfrg::access::IdentityMap = toml::from_str(&text)?;
    maps.maps()?;
    let state = match fs::read(baseline) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let files = read_observed(observed)?;
    let mut forges = Vec::new();
    for name in frozen {
        forges.push(cfrg::model::Forge::parse(name)?);
    }
    let run_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    Ok((
        maps,
        state,
        files,
        cfrg::access::PlanOptions {
            frozen: forges,
            operator: operator.clone(),
            run_at,
        },
    ))
}

fn role_maps() -> RoleMaps {
    RoleMaps {
        github: &cghb::ROLES,
        forgejo: &cfgj::ROLES,
        gitlab: &cglb::ROLES,
        bitbucket: &cbkt::ROLES,
    }
}

fn grant_table() -> GrantTable {
    GrantTable {
        github: &cghb::GRANTS,
        forgejo: &cfgj::GRANTS,
        gitlab: &cglb::GRANTS,
        bitbucket: &cbkt::GRANTS,
    }
}

fn status_targets() -> StatusTargets {
    StatusTargets {
        forgejo: &cfgj::STATUS,
        gitlab: &cglb::STATUS,
        bitbucket: &cbkt::STATUS,
    }
}

fn collect_source(options: &collect::Options) -> Result<&'static dyn collect::EvidenceSource> {
    match options.forge {
        Forge::Github => Ok(&cghb::EVIDENCE),
        Forge::Forgejo => Ok(&cfgj::EVIDENCE),
        _ => Err("Evidence collection supports github and forgejo only".into()),
    }
}

/// Pure offline plan. Never writes; prints the exact intended API calls.
fn access_plan(
    identities: &PathBuf,
    baseline: &PathBuf,
    observed: &[String],
    operator: &Option<String>,
    frozen: &[String],
) -> Result<cfrg::access::AccessPlan> {
    let (maps, state, files, options) =
        access_inputs(identities, baseline, observed, operator, frozen)?;
    cfrg::access::plan(
        &maps,
        state.as_ref(),
        &files,
        &options,
        &role_maps(),
        &grant_table(),
    )
}

/// Baseline advancement. `--initialize` bootstraps from current observations
/// without planning writes; otherwise a converged plan refreshes the state
/// and anything else runs the (currently unwired) transport to fail closed.
// Eight explicit CLI inputs kept separate so each flag stays visible at the
// dispatch site instead of hiding in a struct.
#[allow(clippy::too_many_arguments)]
fn access_apply(
    identities: &PathBuf,
    baseline: &PathBuf,
    observed: &[String],
    operator: &Option<String>,
    frozen: &[String],
    initialize: bool,
    state_out: &Option<PathBuf>,
    pace_ms: u64,
) -> Result<()> {
    let (maps, state, files, options) =
        access_inputs(identities, baseline, observed, operator, frozen)?;
    let destination = state_out.clone().unwrap_or_else(|| baseline.clone());
    if initialize {
        if state.is_some() {
            return Err("Refusing to initialize over an existing baseline".into());
        }
        let world = observed_world(&maps, &files)?;
        let snapshot = cfrg::access::snapshot_world(&world);
        fs::write(&destination, serde_json::to_string_pretty(&snapshot)?)?;
        println!(
            "{}",
            serde_json::json!({"initialized": true, "state": destination})
        );
        return Ok(());
    }
    let Some(previous) = state else {
        return Err("No baseline state; initialize it first with access apply --initialize".into());
    };
    let plan = cfrg::access::plan(
        &maps,
        Some(&previous),
        &files,
        &options,
        &role_maps(),
        &grant_table(),
    )?;
    if plan.complete {
        let world = observed_world(&maps, &files)?;
        let snapshot = cfrg::access::snapshot_world(&world);
        fs::write(&destination, serde_json::to_string_pretty(&snapshot)?)?;
        println!(
            "{}",
            serde_json::json!({"complete": true, "state": destination})
        );
        return Ok(());
    }
    let validated = maps.maps()?;
    let observed_names: BTreeSet<String> = files
        .iter()
        .map(|file| file.forge.as_str().to_owned())
        .collect();
    let mut transport = cfrg::access::UnwiredTransport;
    let report = cfrg::access::apply_plan(
        &plan,
        &mut transport,
        Duration::from_millis(pace_ms),
        &validated,
        &observed_names,
        &previous,
    );
    println!("{}", serde_json::to_string_pretty(&report)?);
    Err("Live forge transport is unwired; baseline unchanged".into())
}

/// Canonicalize snapshots outside the planner for baseline snapshots.
fn observed_world(
    maps: &cfrg::access::IdentityMap,
    files: &[cfrg::access::ObservedFile],
) -> Result<cfrg::access::World> {
    let validated = maps.maps()?;
    let (world, _, _, _) = cfrg::access::observe(&validated, files, &role_maps())?;
    Ok(world)
}

fn run(action: Action) -> Result<()> {
    match action {
        Action::Validate { policy } => {
            let policy = read(policy)?;
            println!(
                "{}",
                serde_json::json!({"valid":true,"repositories":policy.repositories.len()})
            );
        }
        Action::Sync {
            policy,
            repository,
            refs,
            all_refs,
            destinations,
            apply,
            timeout,
        } => {
            let report = cfrg::sync::reconcile(
                &read(policy)?,
                &repository,
                &refs,
                all_refs,
                &destinations,
                apply,
                Duration::from_secs(timeout),
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            let blocked = report.replicas.iter().any(|replica| {
                matches!(
                    replica.state,
                    cfrg::sync::State::Blocked | cfrg::sync::State::Pending
                )
            });
            let source_blocked = matches!(
                report.source_state,
                cfrg::sync::State::Blocked | cfrg::sync::State::Pending
            );
            if source_blocked || blocked || (apply && !report.completed()) {
                return Err("Reconciliation is incomplete; inspect the JSON report".into());
            }
        }
        Action::Access { action } => match action {
            AccessAction::Plan {
                identities,
                baseline,
                observed,
                operator,
                frozen,
            } => {
                let plan = access_plan(&identities, &baseline, &observed, &operator, &frozen)?;
                println!("{}", serde_json::to_string_pretty(&plan)?);
                if !plan.complete {
                    return Err("Access plan is not converged; inspect the JSON report".into());
                }
            }
            AccessAction::Apply {
                identities,
                baseline,
                observed,
                operator,
                frozen,
                initialize,
                state_out,
                pace_ms,
            } => {
                access_apply(
                    &identities,
                    &baseline,
                    &observed,
                    &operator,
                    &frozen,
                    initialize,
                    &state_out,
                    pace_ms,
                )?;
            }
        },
        Action::Plan { policy, repository } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&read(policy)?.plan(&repository)?)?
            );
        }
        Action::Decide {
            policy,
            repository,
            observations,
            capabilities,
        } => {
            let states: BTreeMap<String, RunState> =
                serde_json::from_slice(&fs::read(observations)?)?;
            println!(
                "{}",
                serde_json::to_string(&execution_decision(
                    &read(policy)?,
                    &repository,
                    &capabilities,
                    &states
                )?)?
            );
        }
        Action::Clone {
            policy,
            repository,
            commit,
            destination,
            timeout,
        } => {
            if ![40, 64].contains(&commit.len())
                || !commit
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            {
                return Err(
                    "Clone requires a complete lowercase commit identity, never a moving branch"
                        .into(),
                );
            }
            let plan = read(policy)?.plan(&repository)?;
            // create_dir is the exclusive ownership claim. Existing data, even
            // an empty directory or dangling symlink, is never replaced.
            fs::create_dir(&destination)?;
            let destination = destination.canonicalize()?;
            let result = clone_sources(&plan.clone_urls, &commit, &destination, timeout);
            if result.is_err() {
                let _ = fs::remove_dir_all(&destination);
            }
            result?;
        }
        Action::Status(_)
        | Action::Bridge(_)
        | Action::Collect(_)
        | Action::SourceRevision
        | Action::Resolve { .. }
        | Action::CredentialHelper { .. } => return Err("dispatched directly by main".into()),
    }
    Ok(())
}

fn clone_sources(
    urls: &[String],
    commit: &str,
    destination: &std::path::Path,
    timeout: u64,
) -> Result<()> {
    for url in urls {
        // Each failed attempt has its own disposable object database. Never
        // combine incomplete data from different remotes or mutate a mirror.
        let attempt = tempfile::Builder::new()
            .prefix(".cfrg-fetch-")
            .tempdir_in(destination)?;
        let mut environment: Environment = std::env::vars_os().collect();
        environment.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
        environment.insert("GIT_LFS_SKIP_SMUDGE".into(), "1".into());
        let runner = Runner::new(
            attempt.path().into(),
            environment,
            Duration::from_secs(timeout),
        )?;
        let git = |args: &[&str], capture| {
            let argv = [
                "git",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "protocol.file.allow=never",
            ]
            .into_iter()
            .chain(args.iter().copied())
            .map(String::from)
            .collect::<Vec<_>>();
            runner.run(&argv, capture)
        };
        git(&["init", "--quiet"], false)?;
        if git(
            &[
                "fetch",
                "--no-recurse-submodules",
                "--no-tags",
                "--depth=1",
                "--",
                url,
                commit,
            ],
            false,
        )
        .is_err()
        {
            if cfrg::INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("Clone interrupted".into());
            }
            eprintln!("cfrg: exact commit unavailable from {url}; checking next declared source");
            continue;
        }
        if git(&["rev-parse", "FETCH_HEAD^{commit}"], true)?.trim() != commit {
            return Err("Fetched commit identity mismatch".into());
        }
        git(&["checkout", "--quiet", "--detach", commit], false)?;
        git(&["remote", "add", "origin", url], false)?;
        for entry in fs::read_dir(attempt.path())? {
            let entry = entry?;
            fs::rename(entry.path(), destination.join(entry.file_name()))?;
        }
        println!(
            "{}",
            serde_json::json!({"event":"clone","url":url,"commit":commit,"destination":destination,"submodules":false,"lfs":false})
        );
        return Ok(());
    }
    Err("No declared source supplied the requested exact commit".into())
}

fn main() -> ExitCode {
    if let Err(error) = ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst)) {
        eprintln!("cfrg: cannot install cancellation handler: {error}");
        return ExitCode::from(2);
    }
    match Cli::parse().action {
        Action::Status(options) => match status::run(options, &status_targets()) {
            Ok(report) => {
                println!("{report}");
                if report["complete"] == true {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(2)
                }
            }
            Err(error) => {
                eprintln!("cfrg status: {error}");
                ExitCode::from(2)
            }
        },
        Action::Bridge(options) => match bridge::run(options, &cglb::GITLAB, &cfgj::FORGEJO) {
            Ok(report) => {
                println!("{report}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("cfrg bridge: {error}");
                ExitCode::from(2)
            }
        },
        Action::Collect(options) => {
            match collect_source(&options).and_then(|source| collect::run(options, source)) {
                Ok(code) => ExitCode::from(code),
                Err(error) => {
                    eprintln!("cfrg collect: {error}");
                    ExitCode::from(2)
                }
            }
        }
        Action::SourceRevision => {
            println!("{}", cfrg::SOURCE_REVISION);
            ExitCode::SUCCESS
        }
        Action::Resolve { request, emit } => match resolve::run_resolve(request, &emit) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("cfrg resolve: {error}");
                ExitCode::from(2)
            }
        },
        Action::CredentialHelper {
            env,
            expect_origin,
            username,
            action,
        } => match resolve::run_credential_helper(env, expect_origin, username, action) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("cfrg credential-helper: {error}");
                ExitCode::from(2)
            }
        },
        action => match run(action) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("cfrg: {error}");
                ExitCode::from(2)
            }
        },
    }
}
