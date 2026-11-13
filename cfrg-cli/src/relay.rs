//! `cfrg relay apply <event-json>` and `cfrg relay sweep`: the issue relay as a Forgejo Actions job runs it.
//!
//! The mapping lives in the PostgreSQL database named by `CFRG_RELAY_DATABASE` (a libpq connection string; it carries
//! a credential, so it is never an argument). Every outcome is one JSON line on standard output.
use cfrg::{
    contributed::Contributed,
    native::Placement,
    relay::{
        pacing::SystemClock, pg::PgStore, Engine, Forges, Hint, HoldAll, Outcome, RouterConfig,
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
    let config = RouterConfig::parse(&fs::read_to_string(&options.config)?)?;
    let contributed = Contributed::from_placement(
        &Placement::from_document(&fs::read(&options.placement)?)?,
        &[],
    )?;
    let database =
        std::env::var("CFRG_RELAY_DATABASE").map_err(|_| "CFRG_RELAY_DATABASE is not set")?;
    let mut store = PgStore::connect(&database)?;
    // The live drivers (cghb, cglb, cfgj) are added here by the workflow task; a forge without one is an outage.
    let mut forges = Forges::new();
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
