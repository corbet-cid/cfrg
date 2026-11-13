//! `cfrg relay apply <event-json>` and `cfrg relay sweep`: the issue relay, as a job runs it. The work is the
//! `cfrg-relay` library (the same code `cfrg serve` calls in-process).
//!
//! The mapping lives in the PostgreSQL database named by `CFRG_RELAY_DATABASE` (a libpq connection string; it carries
//! a credential, so it is never an argument). The provider credentials are files of the credentials directory
//! (`--credentials-dir`, default `$CREDENTIALS_DIRECTORY`). Every outcome is one JSON line on standard output.
use cfrg::{
    relay::{Hint, Outcome},
    Result,
};
use cfrg_relay::{Applied, Settings, Swept};
use clap::{Args, Subcommand};
use std::{fs, io::Read, path::PathBuf};

#[derive(Args)]
pub struct Options {
    /// The relay configuration (JSON): relay identities, identity table, limits. No repository list.
    #[arg(long, env = "CFRG_RELAY_CONFIG")]
    config: PathBuf,
    /// The placement document: the repository set comes from it, its `contributed` list is asked first (R10, R11).
    #[arg(long, env = "CFRG_PLACEMENT")]
    placement: PathBuf,
    /// The directory of the provider credential files (default: `$CREDENTIALS_DIRECTORY`).
    #[arg(long)]
    credentials_dir: Option<PathBuf>,
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
    let database =
        std::env::var("CFRG_RELAY_DATABASE").map_err(|_| "CFRG_RELAY_DATABASE is not set")?;
    let settings = Settings {
        config: options.config,
        placement: options.placement,
        database,
        credentials_dir: options.credentials_dir,
    };
    match options.action {
        Action::Apply { event } => {
            let hint: Hint = serde_json::from_str(&read_event(&event)?)?;
            match cfrg_relay::apply(&settings, &hint)? {
                Applied::Refused(why) => {
                    eprintln!("relay: refused: {why}");
                    return Err(why.into());
                }
                Applied::Done(report) => {
                    println!("{}", serde_json::to_string(&report)?);
                    if let Outcome::Deferred(why) = &report.outcome {
                        return Err(format!("Nothing was done: {why}").into());
                    }
                }
            }
        }
        Action::Sweep { older_than } => match cfrg_relay::sweep(&settings, older_than)? {
            // Not ready: refuse cleanly with exit 0 (the daily run must not go red for a missing configuration).
            Swept::Refused(why) => eprintln!("relay: refused: {why}"),
            Swept::Done(swept) => println!("{}", serde_json::to_string(&swept)?),
        },
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
