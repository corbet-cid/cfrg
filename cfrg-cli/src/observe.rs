//! `cfrg observe`: read-only facts a caller acts on, answered by the forge.
//! One JSON line on standard output per call.
use crate::contents::open;
use cfrg::{
    land::Capability,
    model::Forge,
    native::Endpoint,
    observe::{self, Observe},
    Result,
};
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Args)]
pub struct Options {
    #[arg(long, value_enum)]
    forge: Forge,
    /// HTTPS origin of the forge: host and optional port only.
    #[arg(long)]
    origin: String,
    /// Environment variable that holds the API token.
    #[arg(long)]
    token_env: String,
    /// Rate-window state; a fresh temporary directory when omitted.
    #[arg(long, env = "CFRG_OBSERVE_STATE_DIR")]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Head commit of a branch (`null` when the branch does not exist).
    Head { repository: String, branch: String },
    /// Latest status of every context for one exact commit.
    Status { repository: String, commit: String },
}

pub fn run(options: Options) -> Result<()> {
    match options.forge {
        Forge::Forgejo => {}
        Forge::Gitlab => return unsupported(cglb::OBSERVE),
        Forge::Bitbucket => return unsupported(cbkt::OBSERVE),
        Forge::Github => return unsupported(cghb::OBSERVE),
    }
    let endpoint = Endpoint {
        origin: options.origin,
        token_env: options.token_env,
    };
    endpoint.validate()?;
    let mut opened = open(options.state_dir)?;
    let line = match options.action {
        Action::Head { repository, branch } => {
            let mut target = cfgj::land::Land::new(&endpoint, &repository, &mut opened.http)?;
            let head = target.branch_head(&branch)?;
            observe::head_line(&repository, &branch, head.as_deref())
        }
        Action::Status { repository, commit } => {
            let mut target = cfgj::land::Land::new(&endpoint, &repository, &mut opened.http)?;
            let statuses = target.commit_statuses(&commit)?;
            observe::status_line(&repository, &commit, &statuses)?
        }
    };
    println!("{line}");
    Ok(())
}

fn unsupported(capability: Capability) -> Result<()> {
    Err(format!(
        "Observe is {:?} for this forge: {}",
        capability.support, capability.note
    )
    .into())
}
