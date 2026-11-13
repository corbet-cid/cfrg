//! The relay's state contract (Bridge design §5): one mapping of linked objects, a lease per object, the intent
//! journal, the cap ledger and the namespace holds. The forges stay the truth (RD10).
//!
//! Every write is an idempotent upsert of the full row, keys are UUIDs made by the writer (pgEdge is multi-master:
//! no sequences). [`MemStore`] is the test store; [`super::pg::PgStore`] is the production one.

use super::model::{
    CommentRow, Hold, Intent, IntentStatus, LedgerEntry, LogicalObject, SiblingRow,
};
use crate::{model::Forge, Result};
use std::collections::BTreeMap;

pub trait Store {
    /// Runs `f` as one transaction: all of its writes happen, or none do. Nesting joins the outer one.
    fn atomically(&mut self, f: &mut dyn FnMut(&mut dyn Store) -> Result<()>) -> Result<()>;

    /// Takes the single-writer lease of one logical object, or renews it for the same holder. `false` = another
    /// holder's lease has not expired yet. Works before the object has a row.
    fn lease(&mut self, lid: &str, holder: &str, now: u64, secs: u64) -> Result<bool>;
    fn unlease(&mut self, lid: &str, holder: &str) -> Result<()>;

    fn object(&self, lid: &str) -> Result<Option<LogicalObject>>;
    fn objects(&self) -> Result<Vec<LogicalObject>>;
    fn put_object(&mut self, object: LogicalObject) -> Result<()>;
    fn sibling_at(&self, forge: Forge, repo: &str, number: u64) -> Result<Option<SiblingRow>>;
    /// Hub first (RD2), then the spokes in a fixed order.
    fn siblings_of(&self, lid: &str) -> Result<Vec<SiblingRow>>;
    fn put_sibling(&mut self, sibling: SiblingRow) -> Result<()>;
    fn comments_of(&self, lid: &str) -> Result<Vec<CommentRow>>;
    fn put_comment(&mut self, comment: CommentRow) -> Result<()>;

    /// Upsert by `intent.id`.
    fn put_intent(&mut self, intent: Intent) -> Result<()>;
    fn intents_of(&self, lid: &str) -> Result<Vec<Intent>>;
    fn intents_open(&self) -> Result<Vec<Intent>>;

    fn ledger_append(&mut self, entry: LedgerEntry) -> Result<()>;
    fn ledger_since(&self, ts: u64) -> Result<Vec<LedgerEntry>>;
    /// Reserves the next write slot of a (forge, namespace) for the caller and returns how many seconds it must wait
    /// before sending: every claim is at least `gap` after the previous claim, and a claim with `create_gap`
    /// (an object creation) is also at least that after the previous creation. One atomic step, so concurrent
    /// jobs on the namespace are spaced by the store, not by their own timers.
    fn pace_claim(
        &mut self,
        forge: Forge,
        namespace: &str,
        now: u64,
        gap: u64,
        create_gap: Option<u64>,
    ) -> Result<u64>;
    fn holds(&self) -> Result<Vec<Hold>>;
    /// `false` = the namespace is already on hold.
    fn hold_put(&mut self, hold: Hold) -> Result<bool>;
    fn hold_release(&mut self, forge: Forge, namespace: &str) -> Result<bool>;
}

pub fn sibling_order(row: &SiblingRow) -> (bool, Forge, u64) {
    (row.forge != Forge::Forgejo, row.forge, row.number)
}

pub fn comment_order(row: &CommentRow) -> (u64, Forge, u64) {
    (row.created_at, row.origin_forge, row.origin_id)
}

#[derive(Clone, Default)]
struct State {
    leases: BTreeMap<String, (String, u64)>,
    objects: BTreeMap<String, LogicalObject>,
    siblings: BTreeMap<String, SiblingRow>,
    comments: BTreeMap<String, CommentRow>,
    intents: BTreeMap<String, Intent>,
    ledger: Vec<LedgerEntry>,
    holds: Vec<Hold>,
    /// (forge, namespace) to (next write, next creation).
    pace: BTreeMap<String, (u64, u64)>,
}

/// [`Store`] in memory, for tests.
#[derive(Default)]
pub struct MemStore {
    state: State,
    depth: usize,
}

impl Store for MemStore {
    fn atomically(&mut self, f: &mut dyn FnMut(&mut dyn Store) -> Result<()>) -> Result<()> {
        let before = (self.depth == 0).then(|| self.state.clone());
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        if let (Err(_), Some(previous)) = (&result, before) {
            self.state = previous;
        }
        result
    }

    fn lease(&mut self, lid: &str, holder: &str, now: u64, secs: u64) -> Result<bool> {
        let free = self
            .state
            .leases
            .get(lid)
            .is_none_or(|(by, until)| by == holder || *until <= now);
        if free {
            self.state.leases.insert(
                lid.to_owned(),
                (holder.to_owned(), now.saturating_add(secs)),
            );
        }
        Ok(free)
    }

    fn unlease(&mut self, lid: &str, holder: &str) -> Result<()> {
        if self
            .state
            .leases
            .get(lid)
            .is_some_and(|(by, _)| by == holder)
        {
            self.state.leases.remove(lid);
        }
        Ok(())
    }

    fn object(&self, lid: &str) -> Result<Option<LogicalObject>> {
        Ok(self.state.objects.get(lid).cloned())
    }

    fn objects(&self) -> Result<Vec<LogicalObject>> {
        Ok(self.state.objects.values().cloned().collect())
    }

    fn put_object(&mut self, object: LogicalObject) -> Result<()> {
        self.state.objects.insert(object.lid.clone(), object);
        Ok(())
    }

    fn sibling_at(&self, forge: Forge, repo: &str, number: u64) -> Result<Option<SiblingRow>> {
        Ok(self
            .state
            .siblings
            .get(&format!("{}/{repo}/{number}", forge.as_str()))
            .cloned())
    }

    fn siblings_of(&self, lid: &str) -> Result<Vec<SiblingRow>> {
        let mut rows: Vec<_> = self
            .state
            .siblings
            .values()
            .filter(|s| s.lid == lid)
            .cloned()
            .collect();
        rows.sort_by_key(sibling_order);
        Ok(rows)
    }

    fn put_sibling(&mut self, sibling: SiblingRow) -> Result<()> {
        self.state.siblings.insert(sibling.key(), sibling);
        Ok(())
    }

    fn comments_of(&self, lid: &str) -> Result<Vec<CommentRow>> {
        let mut rows: Vec<_> = self
            .state
            .comments
            .values()
            .filter(|c| c.lid == lid)
            .cloned()
            .collect();
        rows.sort_by_key(comment_order);
        Ok(rows)
    }

    fn put_comment(&mut self, comment: CommentRow) -> Result<()> {
        self.state.comments.insert(comment.lcid.clone(), comment);
        Ok(())
    }

    fn put_intent(&mut self, intent: Intent) -> Result<()> {
        self.state.intents.insert(intent.id.clone(), intent);
        Ok(())
    }

    fn intents_of(&self, lid: &str) -> Result<Vec<Intent>> {
        Ok(self
            .state
            .intents
            .values()
            .filter(|i| i.lid == lid)
            .cloned()
            .collect())
    }

    fn intents_open(&self) -> Result<Vec<Intent>> {
        Ok(self
            .state
            .intents
            .values()
            .filter(|i| i.status == IntentStatus::Planned)
            .cloned()
            .collect())
    }

    fn ledger_append(&mut self, entry: LedgerEntry) -> Result<()> {
        self.state.ledger.push(entry);
        Ok(())
    }

    fn ledger_since(&self, ts: u64) -> Result<Vec<LedgerEntry>> {
        Ok(self
            .state
            .ledger
            .iter()
            .filter(|e| e.ts >= ts)
            .cloned()
            .collect())
    }

    fn pace_claim(
        &mut self,
        forge: Forge,
        namespace: &str,
        now: u64,
        gap: u64,
        create_gap: Option<u64>,
    ) -> Result<u64> {
        let slot = self
            .state
            .pace
            .entry(format!("{}/{namespace}", forge.as_str()))
            .or_insert((0, 0));
        let start = now.max(slot.0).max(create_gap.map_or(0, |_| slot.1));
        slot.0 = start.saturating_add(gap);
        if let Some(create_gap) = create_gap {
            slot.1 = start.saturating_add(create_gap);
        }
        Ok(start - now)
    }

    fn holds(&self) -> Result<Vec<Hold>> {
        Ok(self.state.holds.clone())
    }

    fn hold_put(&mut self, hold: Hold) -> Result<bool> {
        let known = self
            .state
            .holds
            .iter()
            .any(|h| h.forge == hold.forge && h.namespace == hold.namespace);
        if !known {
            self.state.holds.push(hold);
        }
        Ok(!known)
    }

    fn hold_release(&mut self, forge: Forge, namespace: &str) -> Result<bool> {
        let before = self.state.holds.len();
        self.state
            .holds
            .retain(|h| !(h.forge == forge && h.namespace == namespace));
        Ok(self.state.holds.len() != before)
    }
}
