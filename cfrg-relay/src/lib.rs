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
        contrib::Contrib,
        copyid::CopyIds,
        derive::{repos_from_placement, repos_from_placement_with, unresolved_copies},
        heads::Heads,
        ops::SweepReport,
        pacing::SystemClock,
        pg::PgStore,
        Engine, Hint, HoldAll, Report, RouterConfig,
    },
    Result,
};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub mod heads_live;
pub mod live;
pub mod notices_live;

/// Copy ids resolved from a path in one run, at most; the rest follow in the next runs (reads are cheap, a burst is not).
pub const MAX_RESOLVES_PER_RUN: usize = 50;

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
    let drivers = live::Drivers::open(&dir, &runtime, &p.placement.source.origin);
    let mut p = p;
    resolve_copies(&mut store, &drivers, &runtime, &mut p)?;
    if let Err(why) = p.config.validate() {
        return Ok(refused(why.to_string()));
    }
    let stale = live::Stale::default();
    let mut forges = drivers.into_forges(&runtime, &p.config, &stale);
    let clock = SystemClock;
    let holder = format!("cfrg-relay-{}", std::process::id());
    let mut contrib = live_contrib(&dir, &runtime, &p.placement.source.origin);
    let mut engine = Engine {
        cfg: &p.config,
        store: &mut store,
        forges: &mut forges,
        clock: &clock,
        // Fail closed until the reviewer's abuse screen (R51) is attached: new objects stay on Forgejo.
        screen: &HoldAll,
        contributed: &p.contributed,
        holder: &holder,
        contrib: contrib.as_mut(),
    };
    let result = f(&mut engine);
    // A definitive 404 on a repository: forget its resolved id, so the next run resolves it again.
    for (forge, id) in stale.borrow().iter() {
        store.drop_id(*forge, id)?;
    }
    result
}

/// The write ports of the contribution flow, for the forges whose credential is present. Without the Forgejo
/// credential there is no notice and so no flow at all (`None`: contribution events are reported as unsupported).
pub fn live_contrib(
    dir: &Option<PathBuf>,
    rt: &std::rc::Rc<tokio::runtime::Runtime>,
    forgejo_origin: &str,
) -> Option<Contrib> {
    let notices = live::open_forgejo(dir, forgejo_origin)?;
    let forgejo_token = live::credential(dir, live::FORGEJO_TOKEN);
    let gitlab_token = live::credential(dir, live::GITLAB_TOKEN);
    let host = forgejo_origin
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_owned();
    let git = heads_live::LiveGit {
        rt: rt.clone(),
        github: live::open_github(dir, rt),
        gitlab_token: gitlab_token.clone(),
        forgejo_token,
        forgejo_host: host,
    };
    let mut heads = Heads::new(Box::new(git));
    if let Some(driver) = live::open_forgejo(dir, forgejo_origin) {
        heads.insert(Box::new(heads_live::ForgejoRequests {
            rt: rt.clone(),
            driver,
        }));
    }
    if let (Some(driver), Some(token)) = (live::open_gitlab(dir, rt), gitlab_token) {
        match heads_live::GitlabHeads::new(rt.clone(), driver, "gitlab.com", &token) {
            Ok(host) => heads.insert(Box::new(host)),
            Err(why) => eprintln!("relay: gitlab heads refused: {why}"),
        }
    }
    if let Some(driver) = live::open_github(dir, rt) {
        match heads_live::GithubHeads::new(rt.clone(), driver) {
            Ok(host) => heads.insert(Box::new(host)),
            Err(why) => eprintln!("relay: github heads refused: {why}"),
        }
    }
    Some(Contrib {
        heads,
        notices: Box::new(notices_live::ForgejoNotices {
            driver: notices,
            rt: rt.clone(),
        }),
    })
}

/// Take-in of a contribution (merge): the owner pushed `reviewed_sha`; every sibling and the notice are closed as merged.
pub fn take_in(settings: &Settings, hint: &Hint, reviewed_sha: &str) -> Result<Applied> {
    run(settings, Applied::Refused, |engine| {
        Ok(Applied::Done(
            engine.take_in_contribution(hint, reviewed_sha)?,
        ))
    })
}

/// Fills the ids of the copies the placement does not know: from the database, else once from the path through the
/// driver, then stored. A copy that cannot be resolved now (outage, no credential) or does not exist is left out
/// of this run's set; nothing is guessed.
fn resolve_copies(
    store: &mut PgStore,
    drivers: &live::Drivers,
    runtime: &tokio::runtime::Runtime,
    p: &mut Prepared,
) -> Result<()> {
    let mut known = store.load()?;
    let missing = unresolved_copies(&p.placement, &p.contributed, &|f, path| {
        known.get(&(f, path.to_owned())).cloned()
    });
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    for (forge, path) in missing.into_iter().take(MAX_RESOLVES_PER_RUN) {
        match drivers.resolve(runtime, forge, &path) {
            Ok(Some(id)) => {
                store.put(forge, &path, &id, now)?;
                known.insert((forge, path), id);
            }
            Ok(None) => eprintln!("relay: {} copy {path} does not exist (404)", forge.as_str()),
            Err(why) => eprintln!("relay: {} copy {path} not resolved: {why}", forge.as_str()),
        }
    }
    p.config.repos = repos_from_placement_with(&p.placement, &p.contributed, &|f, path| {
        known.get(&(f, path.to_owned())).cloned()
    });
    Ok(())
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
