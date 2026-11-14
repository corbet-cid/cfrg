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
    /// Waits `secs` seconds, once. Tests move their clock instead of waiting.
    fn sleep(&self, secs: u64);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn sleep(&self, secs: u64) {
        std::thread::sleep(std::time::Duration::from_secs(secs));
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

/// Takes the next write slot of the namespace and waits for it, once: the wait is computed by the store from
/// the other jobs' claims, then slept in one piece. `creation` also keeps the longer gap between creations.
pub fn pace_write(
    store: &mut dyn Store,
    cfg: &RouterConfig,
    clock: &dyn Clock,
    forge: Forge,
    namespace: &str,
    creation: bool,
) -> Result<u64> {
    let p = &cfg.pacing;
    let wait = store.pace_claim(
        forge,
        namespace,
        clock.now(),
        p.write_gap_secs,
        creation.then_some(p.create_gap_secs),
    )?;
    if wait > 0 {
        clock.sleep(wait);
    }
    Ok(wait)
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

        fn sleep(&self, secs: u64) {
            self.advance_secs(secs);
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

    #[test]
    fn claims_are_spaced_per_namespace_and_creations_by_the_longer_gap() {
        let mut store = MemStore::default();
        let claim = |s: &mut MemStore, ns: &str, now: u64, create: bool| {
            s.pace_claim(Forge::Github, ns, now, 2, create.then_some(12))
                .unwrap()
        };
        assert_eq!(claim(&mut store, "a", 100, true), 0);
        // A write right after waits for the 2 s gap; a creation waits for the 12 s one.
        assert_eq!(claim(&mut store, "a", 100, false), 2);
        assert_eq!(claim(&mut store, "a", 100, true), 12);
        // Another namespace and another forge are independent.
        assert_eq!(claim(&mut store, "b", 100, true), 0);
        assert_eq!(
            store.pace_claim(Forge::Gitlab, "a", 100, 2, None).unwrap(),
            0
        );
        // After the slots have passed there is nothing to wait for.
        assert_eq!(claim(&mut store, "a", 200, true), 0);
    }

    #[test]
    fn two_concurrent_writers_on_one_namespace_are_at_least_a_gap_apart() {
        use std::sync::{Arc, Mutex};
        // Deterministic: every writer claims at the SAME logical second from its own thread, so the waits the store hands out ARE the
        // schedule (a writer that sleeps its wait wakes at now + wait). No wall clock, no sleep: the spacing is asserted on the claims.
        const NOW: u64 = 1_000;
        const GAP: u64 = 1;
        let store = Arc::new(Mutex::new(MemStore::default()));
        let mut waits: Vec<u64> = (0..3)
            .map(|_| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    store
                        .lock()
                        .unwrap()
                        .pace_claim(Forge::Forgejo, "org", NOW, GAP, None)
                        .unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        waits.sort_unstable();
        assert_eq!(waits[0], 0, "the first writer goes at once: {waits:?}");
        for pair in waits.windows(2) {
            assert!(
                pair[1] - pair[0] >= GAP,
                "writers closer than the gap: {waits:?}"
            );
        }
    }
}
