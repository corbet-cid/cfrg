//! RD11 / RF9: caps, holds and the kill switch. Plus the counted limits of R50, R51 and RF8.
//!
//! Semantics follow `cfrg gate` (R9): the first refusal from a provider puts that (forge, namespace) on
//! hold, the hold is never lifted by the relay, and a kill switch directory stops everything. The
//! clock is a parameter so tests run in zero real time.

use super::model::{Hold, LedgerEntry, LedgerKind, Lid, RouterConfig};
use crate::{model::Forge, Result, INTERRUPTED};
use std::{
    sync::atomic::Ordering,
    time::{SystemTime, UNIX_EPOCH},
};

use super::store::Store;

pub const HOUR: u64 = 3_600;
pub const DAY: u64 = 86_400;

pub trait Clock {
    /// Seconds since the epoch.
    fn now(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Why a request may not be made now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Denied {
    KillSwitch,
    /// The namespace is on hold; only the owner lifts it.
    Held(String),
    HourlyCap,
    DailyCap,
}

impl Denied {
    pub fn text(&self) -> String {
        match self {
            Self::KillSwitch => "kill switch is set".into(),
            Self::Held(reason) => format!("namespace is on hold: {reason}"),
            Self::HourlyCap => "hourly mutation cap of the namespace is used".into(),
            Self::DailyCap => "daily mutation cap of the namespace is used".into(),
        }
    }

    /// A hold or the kill switch stops reads too; a cap only stops writes.
    pub fn stops_reads(&self) -> bool {
        matches!(self, Self::KillSwitch | Self::Held(_))
    }
}

fn active_hold(store: &dyn Store, forge: Forge, namespace: &str) -> Result<Option<Hold>> {
    Ok(store
        .holds()?
        .into_iter()
        .find(|h| h.forge == forge && h.namespace == namespace))
}

/// May a request (a mutation if `mutation`) be made to `namespace` on `forge` now?
pub fn admit(
    store: &dyn Store,
    cfg: &RouterConfig,
    forge: Forge,
    namespace: &str,
    mutation: bool,
    now: u64,
) -> Result<std::result::Result<(), Denied>> {
    if cfg.kill_switch.as_ref().is_some_and(|p| p.exists()) {
        return Ok(Err(Denied::KillSwitch));
    }
    if let Some(hold) = active_hold(store, forge, namespace)? {
        return Ok(Err(Denied::Held(hold.reason)));
    }
    if mutation {
        let writes = |window: u64| -> Result<u64> {
            Ok(store
                .ledger_since(now.saturating_sub(window).saturating_add(1))?
                .iter()
                .filter(|e| {
                    e.kind == LedgerKind::Write && e.forge == forge && e.namespace == namespace
                })
                .count() as u64)
        };
        if writes(HOUR)? >= cfg.pacing.namespace_per_hour {
            return Ok(Err(Denied::HourlyCap));
        }
        if writes(DAY)? >= cfg.pacing.namespace_per_day {
            return Ok(Err(Denied::DailyCap));
        }
    }
    Ok(Ok(()))
}

fn count_since(
    store: &dyn Store,
    now: u64,
    window: u64,
    keep: impl Fn(&LedgerEntry) -> bool,
) -> Result<u64> {
    Ok(store
        .ledger_since(now.saturating_sub(window).saturating_add(1))?
        .iter()
        .filter(|e| keep(e))
        .count() as u64)
}

/// RF8: passes that wrote to this object within the last hour.
pub fn write_passes(store: &dyn Store, lid: &Lid, now: u64) -> Result<u64> {
    count_since(store, now, HOUR, |e| {
        e.kind == LedgerKind::WritePass && e.lid.as_deref() == Some(lid.as_str())
    })
}

/// R51: new objects of one author (`forge:id`) fanned out within the last day.
pub fn author_fanouts(store: &dyn Store, author_key: &str, now: u64) -> Result<u64> {
    count_since(store, now, DAY, |e| {
        e.kind == LedgerKind::FanOut && e.detail == author_key
    })
}

/// R50: cascaded deletions within the last hour, fleet-wide and for one primary repository.
pub fn cascades(store: &dyn Store, primary: &str, now: u64) -> Result<(u64, u64)> {
    let all = count_since(store, now, HOUR, |e| e.kind == LedgerKind::Cascade)?;
    let repo = count_since(store, now, HOUR, |e| {
        e.kind == LedgerKind::Cascade && e.detail == primary
    })?;
    Ok((all, repo))
}

#[cfg(test)]
pub(crate) mod testing {
    use super::Clock;
    use std::{cell::Cell, rc::Rc};

    /// A clock the test moves by hand. Cloneable so a test can keep a handle.
    #[derive(Clone)]
    pub struct FakeClock(pub Rc<Cell<u64>>);

    impl FakeClock {
        pub fn at(seconds: u64) -> Self {
            Self(Rc::new(Cell::new(seconds)))
        }

        pub fn advance_secs(&self, secs: u64) {
            self.0.set(self.0.get() + secs);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> u64 {
            self.0.get()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::store::MemStore;

    #[test]
    fn caps_hold_and_kill_switch_decide_before_any_request() {
        let mut cfg = RouterConfig::default();
        cfg.pacing.namespace_per_hour = 2;
        let mut store = MemStore::default();
        let now = 100_000;
        let write = |ts: u64| LedgerEntry {
            ts,
            kind: LedgerKind::Write,
            forge: Forge::Github,
            namespace: "org".into(),
            lid: None,
            detail: String::new(),
        };
        assert_eq!(
            admit(&store, &cfg, Forge::Github, "org", true, now).unwrap(),
            Ok(())
        );
        store.ledger_append(write(now - 10)).unwrap();
        store.ledger_append(write(now - 20)).unwrap();
        assert_eq!(
            admit(&store, &cfg, Forge::Github, "org", true, now).unwrap(),
            Err(Denied::HourlyCap)
        );
        // Reads are not capped, other namespaces are not affected, an old write falls out of the hour.
        assert_eq!(
            admit(&store, &cfg, Forge::Github, "org", false, now).unwrap(),
            Ok(())
        );
        assert_eq!(
            admit(&store, &cfg, Forge::Github, "other", true, now).unwrap(),
            Ok(())
        );
        assert_eq!(
            admit(&store, &cfg, Forge::Github, "org", true, now + HOUR).unwrap(),
            Ok(())
        );
        store
            .hold_put(Hold {
                forge: Forge::Github,
                namespace: "org".into(),
                reason: "429".into(),
                since: now,
            })
            .unwrap();
        let denied = admit(&store, &cfg, Forge::Github, "org", false, now + 10 * DAY).unwrap();
        assert_eq!(denied, Err(Denied::Held("429".into())));
        assert!(denied.unwrap_err().stops_reads());
        let dir = tempfile::tempdir().unwrap();
        cfg.kill_switch = Some(dir.path().join("stop"));
        assert_eq!(
            admit(&store, &cfg, Forge::Gitlab, "x", true, now).unwrap(),
            Ok(())
        );
        std::fs::create_dir(dir.path().join("stop")).unwrap();
        assert_eq!(
            admit(&store, &cfg, Forge::Gitlab, "x", false, now).unwrap(),
            Err(Denied::KillSwitch)
        );
    }
}
