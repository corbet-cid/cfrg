//! The relay engine: admission of one event, the lease, the guarded request plumbing and the intent journal (RF1, RD4, RD11).
//!
//! The passes themselves live in [`super::pass`] (convergence), [`super::cascade`] (R50 deletions) and
//! [`super::ops`] (sweep, release).

use super::forge::{ForgeError, Forges, RouterForge, Screen};
use super::model::{
    digest, lid_object, Actor, Hint, HintAction, Hold, Intent, IntentStatus, Kind, LedgerEntry,
    LedgerKind, Lid, OpKind, RepoSet, RouterConfig, Target,
};
use super::pacing::{self, Clock, Denied};
use super::store::Store;
use crate::{contributed::Contributed, model::Forge, Result};
use serde::Serialize;

/// How a pass ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", content = "detail", rename_all = "snake_case")]
pub enum Outcome {
    /// Every sibling shows the canon; nothing is left to write.
    Converged,
    /// Intents are left pending (an outage, a cap, a hold on one forge, a refusal).
    Pending(String),
    /// The hint is not for the router (unmapped repository, the relay's own action, a confidential issue).
    Dropped(String),
    /// Nothing was read or written: the kill switch or a hold stood in the way.
    Deferred(String),
    /// The object is on hold (R51, RF8, a held deletion).
    Held(String),
    /// A deletion was carried to the other copies (R50).
    Deleted,
    /// The flow is not attached to this engine (no contribution ports).
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    pub lid: Option<Lid>,
    #[serde(flatten)]
    pub outcome: Outcome,
    pub reads: u32,
    pub writes: u32,
    pub pending: u32,
    pub notes: Vec<String>,
}

impl Report {
    pub(super) fn new(lid: Option<Lid>) -> Self {
        Self {
            lid,
            outcome: Outcome::Converged,
            reads: 0,
            writes: 0,
            pending: 0,
            notes: Vec::new(),
        }
    }

    pub(super) fn with(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }

    pub(super) fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }
}

/// A read that did not produce an answer.
pub(super) enum Failed {
    Denied(Denied),
    Error(ForgeError),
}

pub(super) enum Sent<T> {
    Done {
        intent: String,
        value: T,
    },
    /// A cap, a hold or the kill switch: the intent stays pending.
    Skipped(String),
    Unavailable(String),
    Rejected(String),
    /// The provider refused; the namespace is now on hold.
    Held(String),
}

pub struct Engine<'a> {
    pub cfg: &'a RouterConfig,
    pub store: &'a mut dyn Store,
    pub forges: &'a mut Forges,
    pub clock: &'a dyn Clock,
    pub screen: &'a dyn Screen,
    /// R10, R11: asked before any read or write for a repository.
    pub contributed: &'a Contributed,
    /// Names this writer in the lease of an object.
    pub holder: &'a str,
    /// Heads and notices of the contribution flow (RL4, RL5); `None` = contributions are not handled.
    pub contrib: Option<&'a mut super::contrib::Contrib>,
}

impl Engine<'_> {
    /// RD4(a) + the policy check (only listed repositories are relayed, R2/R11/RD12) + R11. No read, no write.
    fn admit_hint(&self, hint: &Hint) -> std::result::Result<(), String> {
        let Some(set) = self.cfg.locate(hint.forge, &hint.repo) else {
            return Err("repository is not relayed".into());
        };
        if let Err(error) = self.refuse_contributed(set) {
            return Err(error.to_string());
        }
        if hint
            .actor
            .as_ref()
            .is_some_and(|a| self.cfg.is_relay(hint.forge, a))
        {
            return Err("the actor is the relay identity".into());
        }
        Ok(())
    }

    /// R11: contributed projects are never relayed. Checked for the primary and every copy before any write.
    pub(super) fn refuse_contributed(&self, set: &RepoSet) -> Result<()> {
        for repo in set.members.values() {
            self.contributed.refuse("relay", &repo.host, &repo.path)?;
        }
        Ok(())
    }

    /// One event: policy and the R10/R11 refusal first (no read, no write), then the lease of its logical object,
    /// then its pass (RD1: the hint only names the object). A second delivery of the same event converges to
    /// the same state and writes nothing (RF4).
    pub fn apply(&mut self, hint: &Hint) -> Result<Report> {
        if let Err(reason) = self.admit_hint(hint) {
            return Ok(Report::new(None).with(Outcome::Dropped(reason)));
        }
        if hint.kind == Kind::Contribution && !Self::hint_is_deletion(hint) {
            return self.apply_contribution(hint);
        }
        let known = self.store.sibling_at(hint.forge, &hint.repo, hint.number)?;
        if known.is_none() && hint.kind != Kind::Issue {
            return Ok(Report::new(None).with(Outcome::Unsupported(
                "a deleted contribution that was never tracked".into(),
            )));
        }
        let lid = known.map_or_else(|| lid_object(&hint.object()), |row| row.lid);
        self.led(&lid, |engine| {
            match engine
                .store
                .sibling_at(hint.forge, &hint.repo, hint.number)?
            {
                Some(row) => engine.pass(&row.lid, Some(hint), None),
                None => engine.start_object(hint),
            }
        })
    }

    /// Runs `work` under the single-writer lease of `lid`; a busy lease defers the event (a Forgejo Actions re-run
    /// or the sweep tries again).
    pub(super) fn led(
        &mut self,
        lid: &str,
        work: impl FnOnce(&mut Self) -> Result<Report>,
    ) -> Result<Report> {
        let now = self.clock.now();
        if !self
            .store
            .lease(lid, self.holder, now, self.cfg.limits.lease_secs)?
        {
            return Ok(Report::new(Some(lid.to_owned()))
                .with(Outcome::Deferred("another writer holds this object".into())));
        }
        let result = work(self);
        self.store.unlease(lid, self.holder)?;
        result
    }

    // --- guarded requests ---------------------------------------------------------------------

    /// One read request: kill switch and holds first, then the gap, then the call. A provider refusal
    /// puts the namespace on hold (RD11).
    pub(super) fn read_call<T>(
        &mut self,
        forge: Forge,
        namespace: &str,
        rep: &mut Report,
        call: impl FnOnce(&mut dyn RouterForge) -> std::result::Result<T, ForgeError>,
    ) -> Result<std::result::Result<T, Failed>> {
        let now = self.clock.now();
        if let Err(denied) = pacing::admit(&*self.store, self.cfg, forge, namespace, false, now)? {
            return Ok(Err(Failed::Denied(denied)));
        }
        rep.reads += 1;
        let Some(adapter) = self.forges.get(forge) else {
            return Ok(Err(Failed::Error(ForgeError::Unavailable(format!(
                "no adapter for {}",
                forge.as_str()
            )))));
        };
        match call(adapter) {
            Ok(value) => Ok(Ok(value)),
            Err(error) => {
                self.after_error(forge, namespace, &error)?;
                Ok(Err(Failed::Error(error)))
            }
        }
    }

    /// The first refusal holds that (forge, namespace); only the owner lifts it (RD11).
    pub(super) fn after_error(
        &mut self,
        forge: Forge,
        namespace: &str,
        error: &ForgeError,
    ) -> Result<()> {
        if let ForgeError::Refused(kind, detail) = error {
            let reason = format!("{} {kind:?}: {detail}", forge.as_str());
            self.store.hold_put(Hold {
                forge,
                namespace: namespace.to_owned(),
                reason,
                since: self.clock.now(),
            })?;
        }
        Ok(())
    }

    /// The open intent of a slot, refreshed, or a new one. Committed before any request (§6).
    pub(super) fn open_intent(
        &mut self,
        lid: &str,
        target: &Target,
        op: OpKind,
        payload: &str,
    ) -> Result<Intent> {
        let now = self.clock.now();
        let digest = digest(&[lid, &format!("{op:?}"), &format!("{target:?}"), payload]);
        let existing = self
            .store
            .intents_of(lid)?
            .into_iter()
            .find(|i| i.status == IntentStatus::Planned && i.same_slot(target, op));
        if let Some(mut intent) = existing {
            if intent.digest != digest {
                intent.digest = digest;
                self.store.put_intent(intent.clone())?;
            }
            return Ok(intent);
        }
        let intent = Intent {
            id: uuid::Uuid::new_v4().to_string(),
            lid: lid.to_owned(),
            target: target.clone(),
            op,
            digest,
            created: now,
            attempts: 0,
            status: IntentStatus::Planned,
            resolved: None,
            note: None,
        };
        self.store.put_intent(intent.clone())?;
        Ok(intent)
    }

    pub(super) fn settle(
        &mut self,
        id: &str,
        lid: &str,
        status: IntentStatus,
        note: Option<String>,
    ) -> Result<()> {
        let now = self.clock.now();
        if let Some(mut intent) = self.store.intents_of(lid)?.into_iter().find(|i| i.id == id) {
            intent.status = status;
            intent.resolved = Some(now);
            intent.note = note.or(intent.note);
            self.store.put_intent(intent)?;
        }
        Ok(())
    }

    fn note_intent(&mut self, id: &str, lid: &str, note: String) -> Result<()> {
        if let Some(mut intent) = self.store.intents_of(lid)?.into_iter().find(|i| i.id == id) {
            intent.note = Some(note);
            self.store.put_intent(intent)?;
        }
        Ok(())
    }

    /// One mutation request for an already opened intent: caps, hold and kill switch, the gap, the
    /// ledger entry (counted before it is sent, so a crash only over-counts), then the call.
    /// The intent is left `Planned` on every outcome but a permanent rejection; the caller settles it
    /// together with the rows the write changed, in one transaction.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn send<T>(
        &mut self,
        lid: &str,
        intent: &Intent,
        forge: Forge,
        namespace: &str,
        rep: &mut Report,
        call: impl FnOnce(&mut dyn RouterForge) -> std::result::Result<T, ForgeError>,
    ) -> Result<Sent<T>> {
        let now = self.clock.now();
        if let Err(denied) = pacing::admit(&*self.store, self.cfg, forge, namespace, true, now)? {
            let text = denied.text();
            self.note_intent(&intent.id, lid, text.clone())?;
            return Ok(Sent::Skipped(text));
        }
        // The gap comes after the checks and before the ledger entry, so the entry carries the send time.
        pacing::pace_write(
            &mut *self.store,
            self.cfg,
            self.clock,
            forge,
            namespace,
            intent.op == OpKind::CreateObject,
        )?;
        let at = self.clock.now();
        let mut attempt = intent.clone();
        attempt.attempts += 1;
        self.store.atomically(&mut |tx| {
            tx.ledger_append(LedgerEntry {
                ts: at,
                kind: LedgerKind::Write,
                forge,
                namespace: namespace.to_owned(),
                lid: Some(lid.to_owned()),
                detail: String::new(),
            })?;
            tx.put_intent(attempt.clone())
        })?;
        rep.writes += 1;
        let Some(adapter) = self.forges.get(forge) else {
            let text = format!("no adapter for {}", forge.as_str());
            self.note_intent(&intent.id, lid, text.clone())?;
            return Ok(Sent::Unavailable(text));
        };
        match call(adapter) {
            Ok(value) => Ok(Sent::Done {
                intent: intent.id.clone(),
                value,
            }),
            Err(error) => {
                self.after_error(forge, namespace, &error)?;
                match error {
                    ForgeError::Refused(kind, detail) => {
                        let text = format!("refused ({kind:?}): {detail}");
                        self.note_intent(&intent.id, lid, text.clone())?;
                        Ok(Sent::Held(text))
                    }
                    ForgeError::Unavailable(detail) => {
                        self.note_intent(&intent.id, lid, format!("unavailable: {detail}"))?;
                        Ok(Sent::Unavailable(detail))
                    }
                    ForgeError::Rejected(detail) => {
                        self.settle(&intent.id, lid, IntentStatus::Failed, Some(detail.clone()))?;
                        Ok(Sent::Rejected(detail))
                    }
                }
            }
        }
    }

    /// Is the post's author the relay identity of that forge?
    pub(super) fn relay_authored(&self, forge: Forge, author: &Actor) -> bool {
        self.cfg.is_relay(forge, author)
    }

    pub(super) fn hint_is_deletion(hint: &Hint) -> bool {
        matches!(
            hint.action,
            HintAction::Deleted | HintAction::CommentDeleted { .. }
        )
    }
}
