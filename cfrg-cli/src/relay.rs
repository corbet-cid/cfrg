//! `cfrg relay apply <event-json>` and `cfrg relay sweep`: the issue relay as a Forgejo Actions job runs it.
//!
//! The mapping lives in the PostgreSQL database named by `CFRG_RELAY_DATABASE` (a libpq connection string; it carries
//! a credential, so it is never an argument). Every outcome is one JSON line on standard output.
use crate::relay_live;
use cfrg::{
    contributed::Contributed,
    native::Placement,
    relay::{
        derive::repos_from_placement, pacing::SystemClock, pg::PgStore, Engine, Hint, HoldAll,
        Outcome, RouterConfig,
    },
    Result,
};
use clap::{Args, Subcommand};
use std::{fs, io::Read, path::PathBuf};

#[derive(Args)]
pub struct Options {
    /// The relay configuration (JSON): replicated repositories, relay identities, identity table, limits.
    #[arg(long, env = "CFRG_RELAY_CONFIG")]
    config: PathBuf,
    /// The placement document; its `contributed` list is asked before any repository is touched (R10, R11).
    #[arg(long, env = "CFRG_PLACEMENT")]
    placement: PathBuf,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Apply one event: a JSON hint naming forge, repository id, object and action, as the text itself, a file, or `-`
    /// for standard input. Exits non-zero when nothing could be done now (busy lease, outage, kill switch) so the job
    /// is re-run.
    Apply { event: String },
    /// Re-drive objects whose intents stayed pending longer than `--older-than` seconds, once each. Exits 0 when
    /// nothing is pending.
    Sweep {
        #[arg(long)]
        older_than: Option<u64>,
    },
}

pub fn run(options: Options) -> Result<()> {
    let mut config = RouterConfig::parse(&fs::read_to_string(&options.config)?)?;
    let placement = Placement::from_document(&fs::read(&options.placement)?)?;
    let contributed = Contributed::from_placement(&placement, &[])?;
    // The repository set is the placement's, never a hand list (R44): every Forgejo primary with a GitHub or GitLab
    // copy, without contributed projects and without Bitbucket.
    if !config.repos.is_empty() {
        return Err(
            "the relay configuration must not list repositories: they come from the placement"
                .into(),
        );
    }
    config.repos = repos_from_placement(&placement, &contributed);
    if let Err(why) = config.validate() {
        // Not ready (typically: the relay identity ids of the forges are not configured yet). The sweep refuses cleanly
        // with exit 0; an apply holds (non-zero) so the job is re-run once the configuration is complete.
        eprintln!("relay: refused: {why}");
        return match options.action {
            Action::Sweep { .. } => Ok(()),
            Action::Apply { .. } => Err(why),
        };
    }
    let database =
        std::env::var("CFRG_RELAY_DATABASE").map_err(|_| "CFRG_RELAY_DATABASE is not set")?;
    let mut store = PgStore::connect(&database)?;
    // One small current-thread runtime for the whole process; a forge without its systemd credential gets no adapter
    // and is an outage for the engine (exit 0 for the sweep, non-zero for apply).
    let runtime = relay_live::runtime()?;
    let mut forges = relay_live::forges(&runtime, &config, &placement.source.origin);
    let clock = SystemClock;
    let holder = format!("cfrg-relay-{}", std::process::id());
    let mut engine = Engine {
        cfg: &config,
        store: &mut store,
        forges: &mut forges,
        clock: &clock,
        // Fail closed until the reviewer's abuse screen (R51) is attached: new objects stay on Forgejo.
        screen: &HoldAll,
        contributed: &contributed,
        holder: &holder,
    };
    match options.action {
        Action::Apply { event } => {
            let hint: Hint = serde_json::from_str(&read_event(&event)?)?;
            let report = engine.apply(&hint)?;
            println!("{}", serde_json::to_string(&report)?);
            if let Outcome::Deferred(why) = &report.outcome {
                return Err(format!("Nothing was done: {why}").into());
            }
        }
        Action::Sweep { older_than } => {
            let swept = engine.sweep(older_than)?;
            println!("{}", serde_json::to_string(&swept)?);
        }
    }
    Ok(())
}

fn read_event(event: &str) -> Result<String> {
    if event.trim_start().starts_with('{') {
        return Ok(event.to_owned());
    }
    if event == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        return Ok(text);
    }
    Ok(fs::read_to_string(event)?)
}
