//! `cfrg serve`: the long-running mode. Signed webhook deliveries (push,
//! pull request) bring a repository's pass forward; a slow periodic sweep
//! covers lost deliveries. Two lanes, one worker each:
//!
//! * `land`: the same idempotent pass `cfrg land --step` runs (merge green
//!   heads, rebase and retest when the base moved, notice merges the forge made
//!   alone, start the hooks). With `follow = "serve"` in the policy,
//!   `cfrg land` only enqueues and this lane does the rest.
//! * `mirror`: read-only verification of the declared push mirrors.
//!
//! `cfrg serve --register [--apply]` reconciles the webhook registration on
//! the forge (organisation hooks plus repository hooks for the rest).
use cfrg::{
    land::Policy,
    native::Placement,
    serve::{self, Lane, Outcome, Reconciler, ServePolicy, Service},
    Result, INTERRUPTED,
};
use clap::Args;
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
};

#[derive(Args)]
pub struct Options {
    /// Declared landing policy (JSON) with a `serve` block.
    #[arg(long, env = "CFRG_LAND_POLICY")]
    policy: Option<PathBuf>,
    /// State directory owned by this process (request state, journal, hook
    /// output, work clones).
    #[arg(long, env = "CFRG_LAND_STATE_DIR")]
    state_dir: Option<PathBuf>,
    /// Reconcile the webhook registration on the forge instead of serving.
    #[arg(long)]
    register: bool,
    /// Write the registration; without it `--register` only plans.
    #[arg(long, requires = "register")]
    apply: bool,
    /// Rewrite the registered hooks even when they match (secret rotation).
    #[arg(long, requires = "register")]
    rotate: bool,
}

pub fn run(options: Options) -> Result<()> {
    let policy = Policy::load(
        options
            .policy
            .as_deref()
            .ok_or("Set --policy or CFRG_LAND_POLICY")?,
    )?;
    crate::land::require_land(policy.forge)?;
    let serve_policy = policy
        .serve
        .clone()
        .ok_or("The policy has no serve block")?;
    let state_dir = options
        .state_dir
        .clone()
        .ok_or("Set --state-dir or CFRG_LAND_STATE_DIR")?;
    fs::create_dir_all(&state_dir)?;
    if options.register {
        return register(&policy, &serve_policy, &state_dir, &options);
    }
    run_server(policy, serve_policy, state_dir)
}

/// The webhook secret: a plain token-like value from the environment.
fn secret(name: &str) -> Result<String> {
    let value = std::env::var(name).map_err(|_| format!("Set {name}"))?;
    if value.len() < 16 || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("The webhook secret must be at least 16 printable characters".into());
    }
    Ok(value)
}

fn register(
    policy: &Policy,
    serve_policy: &ServePolicy,
    state_dir: &Path,
    options: &Options,
) -> Result<()> {
    use cfgj::hooks::{ensure, Scope};
    let webhook = serve_policy
        .webhook
        .as_ref()
        .ok_or("serve.webhook is not declared")?;
    // Planning needs no secret; creating or changing a hook does.
    let secret = if options.apply {
        Some(secret(&serve_policy.secret_env)?)
    } else {
        secret(&serve_policy.secret_env).ok()
    };
    let mut http = crate::land::open_http(state_dir, options.apply)?;
    for org in &webhook.orgs {
        let report = ensure(
            &mut http,
            &policy.endpoint,
            Scope::Org(org),
            &webhook.url,
            secret.as_deref(),
            options.apply,
            options.rotate,
        )?;
        println!("{report}");
    }
    for repository in &policy.repositories {
        let owner = repository.path.split('/').next().unwrap_or_default();
        if webhook.orgs.iter().any(|org| org == owner) {
            continue;
        }
        let report = ensure(
            &mut http,
            &policy.endpoint,
            Scope::Repo(&repository.path),
            &webhook.url,
            secret.as_deref(),
            options.apply,
            options.rotate,
        )?;
        println!("{report}");
    }
    Ok(())
}

struct LandLane {
    policy: Policy,
    state_dir: PathBuf,
    interval: u64,
    sweep: u64,
}

impl Reconciler for LandLane {
    fn reconcile(&mut self, key: &str) -> Result<Outcome> {
        let report = crate::land::pass(&self.policy, &self.state_dir, key, None)?;
        crate::land::print_report(key, &report);
        Ok(Outcome {
            summary: json!({"waiting": report.waiting, "events": report.events.len(), "at": serve::now()}),
            // Waiting work is looked at often (checks finish without an event);
            // an empty queue only on the slow sweep or the next delivery.
            next_in: Some(if report.waiting {
                self.interval
            } else {
                self.sweep
            }),
        })
    }
}

struct MirrorLane {
    placement: PathBuf,
    state: PathBuf,
    sweep: u64,
    /// Apply the renames the primary's names call for after verifying.
    rename: bool,
}

impl Reconciler for MirrorLane {
    fn reconcile(&mut self, key: &str) -> Result<Outcome> {
        let mut report = crate::native::verify(&self.placement, &self.state, Some(key))?;
        let pending = rename_pending(&report);
        if pending && self.rename {
            // The primary was renamed and a destination still carries its old
            // name: follow it, then look again. A refusal (a taken name, a missing
            // credential) is logged and leaves the finding in the report.
            match crate::native::rename(&self.placement, &self.state, Some(key)) {
                Ok(applied) => println!(
                    "{}",
                    json!({"event":"mirror-renamed","repository":key,"complete":applied["complete"]})
                ),
                Err(error) => println!(
                    "{}",
                    json!({"event":"mirror-rename-failed","repository":key,"error":error.to_string()})
                ),
            }
            report = crate::native::verify(&self.placement, &self.state, Some(key))?;
        }
        let complete = report["complete"] == true;
        println!(
            "{}",
            json!({"event":"mirror-verified","repository":key,"complete":complete,"rename_pending":rename_pending(&report)})
        );
        Ok(Outcome {
            summary: json!({"complete": complete, "rename_pending": rename_pending(&report), "at": serve::now()}),
            next_in: Some(self.sweep),
        })
    }
}

/// Whether a verification found a rename of the primary that its destinations
/// (or the declared placement) have not followed.
fn rename_pending(report: &serde_json::Value) -> bool {
    report["repositories"].as_array().is_some_and(|rows| {
        rows.iter().any(|row| {
            matches!(
                row["state"].as_str(),
                Some("rename-pending" | "source-renamed")
            )
        })
    })
}

fn run_server(policy: Policy, serve_policy: ServePolicy, state_dir: PathBuf) -> Result<()> {
    let secret = secret(&serve_policy.secret_env)?;
    // One server per state directory. It owns the request state, so a lock
    // file a killed predecessor left behind is stale by definition.
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state_dir.join("serve.lock"))?;
    owner
        .try_lock()
        .map_err(|_| "Another cfrg serve owns this state directory")?;
    let _ = fs::remove_file(state_dir.join("http.lock"));

    let current = serve::now();
    let land = Lane::new("land");
    let land_repositories: BTreeSet<String> =
        policy.repositories.iter().map(|r| r.path.clone()).collect();
    // First pass for everything shortly after start: catches up on whatever
    // happened while the server was down.
    for (offset, repository) in (1u64..).zip(&land_repositories) {
        land.mark(repository, current + offset);
    }
    let mut mirror = None;
    let mut mirror_repositories = BTreeSet::new();
    let mut mirror_delay = 0;
    if let Some(sweep) = &serve_policy.mirrors {
        let placement = Placement::from_document(&fs::read(&sweep.placement)?)?;
        placement.validate()?;
        let lane = Lane::new("mirror");
        mirror_repositories = placement
            .repositories
            .iter()
            .map(|r| r.path.clone())
            .collect();
        // Spread the first sweep so it never arrives as one burst.
        for (offset, repository) in (0u64..).zip(&mirror_repositories) {
            lane.mark(repository, current + 30 + offset % 600);
        }
        mirror_delay = sweep.delay_seconds;
        mirror = Some(lane);
    }
    let service = Arc::new(Service {
        secret: secret.into_bytes(),
        debounce: serve_policy.debounce_seconds,
        land: Arc::clone(&land),
        land_repositories,
        mirror: mirror.clone(),
        mirror_repositories,
        mirror_delay,
        started: current,
        rejected: AtomicU64::new(0),
        accepted: AtomicU64::new(0),
    });
    let listener = TcpListener::bind(&serve_policy.listen)?;
    println!(
        "{}",
        json!({"event":"serve-listening","listen":serve_policy.listen,"revision":cfrg::SOURCE_REVISION})
    );
    let mut land_lane = LandLane {
        interval: policy.interval_seconds,
        sweep: serve_policy.sweep_seconds,
        policy,
        state_dir,
    };
    let mut mirror_lane = serve_policy.mirrors.as_ref().map(|sweep| MirrorLane {
        placement: PathBuf::from(&sweep.placement),
        state: PathBuf::from(&sweep.state),
        sweep: sweep.sweep_seconds,
        rename: sweep.rename,
    });
    thread::scope(|scope| {
        scope.spawn(|| land.work(&mut land_lane, &INTERRUPTED));
        if let (Some(lane), Some(reconciler)) = (&mirror, mirror_lane.as_mut()) {
            scope.spawn(|| lane.work(reconciler, &INTERRUPTED));
        }
        let result = serve::serve(&listener, &service, &INTERRUPTED);
        // Whatever ended the accept loop, the workers stop with it.
        INTERRUPTED.store(true, Ordering::SeqCst);
        result
    })?;
    println!("{}", json!({"event":"serve-stopped"}));
    // Holding the lock until here is the point.
    drop(owner);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_findings_are_noticed() {
        assert!(rename_pending(
            &json!({"repositories":[{"state":"configured"},{"state":"rename-pending"}]})
        ));
        assert!(rename_pending(
            &json!({"repositories":[{"state":"source-renamed"}]})
        ));
        assert!(!rename_pending(
            &json!({"repositories":[{"state":"checked"}]})
        ));
        assert!(!rename_pending(&json!({"complete":true})));
    }
}
