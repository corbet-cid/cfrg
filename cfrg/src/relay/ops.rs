//! Operations around the passes: `sweep` (RF1) and the owner's release of a held object.

use super::engine::{Engine, Report};
use super::model::{Hint, HintAction, Lid};
use crate::Result;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SweepReport {
    pub objects: usize,
    /// Objects with a pending intent older than the bound that this sweep did not reach (`sweep_max_objects`).
    pub left: usize,
    pub reports: Vec<Report>,
}

impl Engine<'_> {
    /// Re-drives what is stuck: objects with a pending intent older than `older_than` seconds, one pass each,
    /// once. A hold or the kill switch keeps its namespace untouched. Rides the daily fleet run (R49); there is
    /// no timer of its own. Bounded by `sweep_max_objects`. With nothing pending it does nothing and reads nothing.
    pub fn sweep(&mut self, older_than: Option<u64>) -> Result<SweepReport> {
        let cfg = self.cfg;
        let older = older_than.unwrap_or(cfg.limits.sweep_older_than_secs);
        let now = self.clock.now();
        let stale: BTreeSet<Lid> = self
            .store
            .intents_open()?
            .into_iter()
            .filter(|i| i.created.saturating_add(older) <= now)
            .map(|i| i.lid)
            .collect();
        let mut out = SweepReport {
            left: stale.len().saturating_sub(cfg.limits.sweep_max_objects),
            ..SweepReport::default()
        };
        for lid in stale.into_iter().take(cfg.limits.sweep_max_objects) {
            if super::pacing::interrupted() {
                break;
            }
            out.objects += 1;
            let report = self.led(&lid, |engine| engine.pass(&lid, None, None))?;
            out.reports.push(report);
        }
        Ok(out)
    }

    /// The owner releases a held object (R51 hold, RF8 freeze, a held deletion) and the next pass converges it.
    pub fn release_object(&mut self, lid: &str) -> Result<bool> {
        let Some(mut obj) = self.store.object(lid)? else {
            return Ok(false);
        };
        if obj.held.take().is_none() {
            return Ok(false);
        }
        obj.screened = true;
        let hint = Hint {
            forge: obj.origin.forge,
            repo: obj.origin.repo.clone(),
            kind: obj.kind,
            number: obj.origin.number,
            action: HintAction::Reconcile,
            actor: None,
        };
        self.store.put_object(obj)?;
        self.apply(&hint)?;
        Ok(true)
    }
}
