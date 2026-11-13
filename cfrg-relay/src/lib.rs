//! The issue relay as a library: [`apply`] one event and [`sweep`], with the live forge drivers behind them.
//!
//! `cfrg relay` (cfrg-cli) and `cfrg serve` call these functions; both run the same code. Both are SYNCHRONOUS and
//! block (database, HTTP through a private current-thread runtime): from async code call them on a blocking thread
//! (`tokio::task::spawn_blocking`), never directly on a runtime thread.
//!
//! Credentials come only from the files of a credentials directory ([`Settings::credentials_dir`], default
//! `$CREDENTIALS_DIRECTORY`): see [`live`] and `docs/relay.md`. A forge without its credential is refused, not guessed.
use cfrg::{
    contributed::Contributed,
    native::Placement,
    relay::{
        derive::repos_from_placement, ops::SweepReport, pacing::SystemClock, pg::PgStore, Engine,
        Hint, HoldAll, Report, RouterConfig,
    },
    Result,
};
use std::{fs, path::PathBuf};

pub mod live;

/// Everything a run needs. Nothing here is a provider credential.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The relay configuration file (JSON): relay identities, identity table, limits. No repository list.
    pub config: PathBuf,
    /// The placement document: the repository set is derived from it, its `contributed` list is asked first.
    pub placement: PathBuf,
    /// The libpq connection string of the mapping database (it carries the database password).
    pub database: String,
    /// Directory holding the provider credential files; `None` = `$CREDENTIALS_DIRECTORY`.
    pub credentials_dir: Option<PathBuf>,
}

/// The answer of [`apply`].
#[derive(Debug)]
pub enum Applied {
    /// The configuration is not ready (typically: no relay identity ids yet). Nothing was read or written.
    Refused(String),
    /// The engine ran. `Outcome::Deferred` inside means nothing could be done now: the caller should retry later.
    Done(Report),
}

/// The answer of [`sweep`].
#[derive(Debug)]
pub enum Swept {
    Refused(String),
    Done(SweepReport),
}

struct Prepared {
    config: RouterConfig,
    placement: Placement,
    contributed: Contributed,
}

fn prepare(s: &Settings) -> Result<Prepared> {
    let mut config = RouterConfig::parse(&fs::read_to_string(&s.config)?)?;
    let placement = Placement::from_document(&fs::read(&s.placement)?)?;
    let contributed = Contributed::from_placement(&placement, &[])?;
    // The repository set is the placement's, never a hand list (R44).
    if !config.repos.is_empty() {
        return Err(
            "the relay configuration must not list repositories: they come from the placement"
                .into(),
        );
    }
    config.repos = repos_from_placement(&placement, &contributed);
    Ok(Prepared {
        config,
        placement,
        contributed,
    })
}

fn run<T>(
    s: &Settings,
    refused: impl FnOnce(String) -> T,
    f: impl FnOnce(&mut Engine<'_>) -> Result<T>,
) -> Result<T> {
    let p = prepare(s)?;
    // The store is connected first: a refused run still proves the database login.
    let mut store = PgStore::connect(&s.database)?;
    if let Err(why) = p.config.validate() {
        return Ok(refused(why.to_string()));
    }
    let dir = s
        .credentials_dir
        .clone()
        .or_else(live::default_credentials_dir);
    let runtime = live::runtime()?;
    let mut forges = live::forges(&dir, &runtime, &p.config, &p.placement.source.origin);
    let clock = SystemClock;
    let holder = format!("cfrg-relay-{}", std::process::id());
    let mut engine = Engine {
        cfg: &p.config,
        store: &mut store,
        forges: &mut forges,
        clock: &clock,
        // Fail closed until the reviewer's abuse screen (R51) is attached: new objects stay on Forgejo.
        screen: &HoldAll,
        contributed: &p.contributed,
        holder: &holder,
    };
    f(&mut engine)
}

/// Applies one event (a hint: forge, repository id, object, action).
pub fn apply(settings: &Settings, hint: &Hint) -> Result<Applied> {
    run(settings, Applied::Refused, |engine| {
        Ok(Applied::Done(engine.apply(hint)?))
    })
}

/// Re-drives the objects whose intents stayed pending longer than `older_than` seconds (default: the engine's).
pub fn sweep(settings: &Settings, older_than: Option<u64>) -> Result<Swept> {
    run(settings, Swept::Refused, |engine| {
        Ok(Swept::Done(engine.sweep(older_than)?))
    })
}
