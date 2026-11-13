//! R50 deletions: "a deletion by the author or a maintainer on any forge deletes all copies".
//!
//! This is the one cascade the router gates hard (the maintainer 2026-11-13). A copy is deleted only when
//! 1. the hint says `deleted`, AND a read returns a typed definitive *gone* (404/410; a 403, 5xx,
//!    timeout or rename never counts) - for a comment: the object is readable and the comment is not in
//!    its complete list;
//! 2. the deletion was made by the author or by a maintainer of that forge's copy. Where the forge does
//!    not name the actor, or cannot say whether they are a maintainer, the deletion is held for the
//!    reviewer instead of cascading;
//! 3. the caps hold: at most 10 cascaded deletions per hour fleet-wide and 3 per primary repository
//!    (one cascade = one logical deletion); beyond that the deletion is held for the reviewer;
//! 4. a contribution's notice is never deleted: it is closed with "deleted at origin".
//!
//! Every delete is an intent journaled first, paced, holdable and ledgered like any other write. The
//! object is tombstoned before the first delete, so a late event cannot resurrect it.

use super::engine::{Engine, Failed, Outcome, Report, Sent};
use super::forge::ReadOutcome;
use super::merge::Patch;
use super::model::{
    Actor, CommentRow, Hint, HintAction, HoldScope, IntentStatus, IssueState, Kind, LedgerEntry,
    LedgerKind, LogicalObject, ObjState, ObjectHold, ObjectRef, OpKind, RepoSet, SiblingRow,
    Target, Tombstone,
};
use super::pacing;
use crate::{model::Forge, Result};

impl Engine<'_> {
    /// Re-drives the open delete intents of one object (a cascade that was interrupted or blocked).
    /// Returns (done, still pending).
    fn drive(
        &mut self,
        obj: &LogicalObject,
        set: &RepoSet,
        rep: &mut Report,
    ) -> Result<(u32, u32)> {
        let mut intents: Vec<_> = self
            .store
            .intents_of(&obj.lid)?
            .into_iter()
            .filter(|i| {
                i.status == IntentStatus::Planned
                    && matches!(i.op, OpKind::DeleteObject | OpKind::DeleteComment)
            })
            .collect();
        intents.sort_by_key(|i| (i.target.forge != Forge::Forgejo, i.created, i.id.clone()));
        let (mut done, mut pending) = (0, 0);
        let mut blocked: Vec<Forge> = Vec::new();
        for intent in intents {
            let forge = intent.target.forge;
            let Some(member) = set.members.get(&forge) else {
                pending += 1;
                continue;
            };
            if blocked.contains(&forge) || pacing::interrupted() {
                pending += 1;
                continue;
            }
            let ns = member.namespace().to_owned();
            let object = ObjectRef {
                forge,
                repo: intent.target.repo.clone(),
                kind: obj.kind,
                number: intent.target.number,
            };
            let comment_id: Option<u64> =
                intent.target.comment.as_ref().and_then(|c| c.parse().ok());
            let sent = self.send(&obj.lid, &intent, forge, &ns, rep, |a| match comment_id {
                Some(id) => a.delete_comment(&object, id),
                None => a.delete_object(&object),
            })?;
            match sent {
                Sent::Done { intent: id, .. } => {
                    let lid = obj.lid.clone();
                    let target = intent.target.clone();
                    let now = self.clock.now();
                    self.store.atomically(&mut |tx| {
                        match &target.comment {
                            None => {
                                if let Some(mut row) =
                                    tx.sibling_at(target.forge, &target.repo, target.number)?
                                {
                                    row.deleted = true;
                                    tx.put_sibling(row)?;
                                }
                            }
                            Some(c) => {
                                let id: u64 = c.parse().unwrap_or(0);
                                for mut row in tx.comments_of(&lid)? {
                                    row.copies
                                        .retain(|f, copy| !(*f == target.forge && copy.id == id));
                                    tx.put_comment(row)?;
                                }
                            }
                        }
                        if let Some(mut i) = tx.intents_of(&lid)?.into_iter().find(|i| i.id == id) {
                            i.status = IntentStatus::Done;
                            i.resolved = Some(now);
                            tx.put_intent(i)?;
                        }
                        Ok(())
                    })?;
                    done += 1;
                }
                Sent::Rejected(why) => {
                    rep.note(format!("delete rejected on {}: {why}", forge.as_str()))
                }
                Sent::Skipped(why) | Sent::Unavailable(why) | Sent::Held(why) => {
                    rep.note(format!("{} is behind on a deletion: {why}", forge.as_str()));
                    blocked.push(forge);
                    pending += 1;
                }
            }
        }
        Ok((done, pending))
    }

    /// A tombstoned object: only its cascade goes on. Anything else about it is dropped.
    pub(super) fn drive_deletes(
        &mut self,
        obj: &LogicalObject,
        set: &RepoSet,
        mut rep: Report,
    ) -> Result<Report> {
        let (done, pending) = self.drive(obj, set, &mut rep)?;
        rep.pending = pending;
        Ok(if pending > 0 {
            rep.with(Outcome::Pending(format!(
                "{pending} deletion(s) still to carry out"
            )))
        } else if done > 0 {
            rep.with(Outcome::Deleted)
        } else {
            rep.with(Outcome::Dropped(
                "the object was deleted (tombstone)".into(),
            ))
        })
    }

    /// Open comment deletions are carried on at the start of every ordinary pass.
    pub(super) fn drive_deletes_in_flight(
        &mut self,
        obj: &LogicalObject,
        set: &RepoSet,
        mut rep: Report,
    ) -> Result<Report> {
        self.drive(obj, set, &mut rep)?;
        Ok(rep)
    }

    fn hold_for_reviewer(
        &mut self,
        mut obj: LogicalObject,
        rep: Report,
        reason: String,
    ) -> Result<Report> {
        obj.held = Some(ObjectHold {
            reason: reason.clone(),
            scope: HoldScope::Frozen,
            since: self.clock.now(),
        });
        self.store.put_object(obj)?;
        Ok(rep.with(Outcome::Held(reason)))
    }

    /// Who may delete: the author of the post, or a maintainer of that forge's copy. `Ok(None)` = unknown.
    fn deleter_allowed(
        &mut self,
        set: &RepoSet,
        forge: Forge,
        actor: &Actor,
        author: Option<&Actor>,
        rep: &mut Report,
    ) -> Result<Option<bool>> {
        if author.is_some_and(|a| a.id == actor.id) {
            return Ok(Some(true));
        }
        let Some(member) = set.members.get(&forge) else {
            return Ok(None);
        };
        let ns = member.namespace().to_owned();
        let id = member.id.clone();
        Ok(self
            .read_call(forge, &ns, rep, |a| a.is_maintainer(&id, actor))?
            .unwrap_or_default())
    }

    fn cascade_caps(&self, obj: &LogicalObject) -> Result<Option<String>> {
        let (all, repo) = pacing::cascades(&*self.store, &obj.primary, self.clock.now())?;
        let limits = &self.cfg.limits;
        Ok(if all >= limits.cascade_deletes_per_hour {
            Some(format!(
                "{all} deletions were already cascaded this hour (limit {})",
                limits.cascade_deletes_per_hour
            ))
        } else if repo >= limits.cascade_deletes_per_repo_per_hour {
            Some(format!(
                "{repo} deletions were already cascaded this hour for {} (limit {})",
                obj.primary, limits.cascade_deletes_per_repo_per_hour
            ))
        } else {
            None
        })
    }

    pub(super) fn deletion(
        &mut self,
        obj: LogicalObject,
        set: &RepoSet,
        hint: &Hint,
        mut rep: Report,
    ) -> Result<Report> {
        let Some(member) = set.members.get(&hint.forge) else {
            return Ok(rep.with(Outcome::Dropped("repository is not relayed".into())));
        };
        let ns = member.namespace().to_owned();
        let object = hint.object();
        match &hint.action {
            HintAction::Deleted => {
                let Some(row) = self.store.sibling_at(hint.forge, &hint.repo, hint.number)? else {
                    return Ok(rep.with(Outcome::Dropped(
                        "a deleted object that was never tracked".into(),
                    )));
                };
                if row.deleted {
                    return Ok(rep.with(Outcome::Dropped("already deleted".into())));
                }
                // 1. Evidence: a typed, definitive "gone". Nothing else counts.
                match self.read_call(hint.forge, &ns, &mut rep, |a| a.read(&object))? {
                    Ok(ReadOutcome::Gone) => {}
                    Ok(ReadOutcome::Found(_)) => {
                        let again = Hint {
                            action: HintAction::Reconcile,
                            ..hint.clone()
                        };
                        return self.pass(&obj.lid, Some(&again), None);
                    }
                    Err(Failed::Denied(d)) => return Ok(rep.with(Outcome::Deferred(d.text()))),
                    Err(Failed::Error(e)) => {
                        return Ok(rep.with(Outcome::Deferred(format!(
                            "the deletion could not be confirmed: {e}"
                        ))));
                    }
                }
                let author = row.origin.then(|| obj.origin_author.clone());
                self.authorise_and_cascade(obj, set, hint, row, author.as_ref(), rep)
            }
            HintAction::CommentDeleted { comment_id } => {
                let comments = self.store.comments_of(&obj.lid)?;
                let found = comments.into_iter().find(|c| {
                    (c.origin_forge == hint.forge && c.origin_id == *comment_id)
                        || c.copies
                            .get(&hint.forge)
                            .is_some_and(|copy| copy.id == *comment_id)
                });
                let Some(comment) = found else {
                    return Ok(rep.with(Outcome::Dropped(
                        "a deleted comment that was never tracked".into(),
                    )));
                };
                if comment.tombstone {
                    return Ok(rep.with(Outcome::Dropped("already deleted".into())));
                }
                // 1. Evidence: the object reads fine and its complete comment list lacks the comment.
                match self.read_call(hint.forge, &ns, &mut rep, |a| a.read(&object))? {
                    Ok(ReadOutcome::Found(state)) => {
                        if state.comments.iter().any(|c| c.id == *comment_id) {
                            let again = Hint {
                                action: HintAction::Reconcile,
                                ..hint.clone()
                            };
                            return self.pass(&obj.lid, Some(&again), None);
                        }
                    }
                    Ok(ReadOutcome::Gone) => {
                        return Ok(rep.with(Outcome::Deferred(
                            "the object itself is gone; its own deletion event decides".into(),
                        )));
                    }
                    Err(Failed::Denied(d)) => return Ok(rep.with(Outcome::Deferred(d.text()))),
                    Err(Failed::Error(e)) => {
                        return Ok(rep.with(Outcome::Deferred(format!(
                            "the deletion could not be confirmed: {e}"
                        ))));
                    }
                }
                self.cascade_comment(obj, set, hint, comment, rep)
            }
            _ => Ok(rep.with(Outcome::Dropped("not a deletion".into()))),
        }
    }

    /// Steps 2 to 4 for a deleted object.
    fn authorise_and_cascade(
        &mut self,
        mut obj: LogicalObject,
        set: &RepoSet,
        hint: &Hint,
        mut row: SiblingRow,
        author: Option<&Actor>,
        mut rep: Report,
    ) -> Result<Report> {
        // The evidence is real, so the copy that is gone is never written to again.
        row.deleted = true;
        row.behind = Some("deleted".into());
        self.store.put_sibling(row.clone())?;
        let who = match &hint.actor {
            Some(actor) => actor.clone(),
            None => {
                return self.hold_for_reviewer(
                    obj,
                    rep,
                    format!("{} #{} was deleted but the forge did not say by whom; held for the reviewer", hint.forge.as_str(), hint.number),
                );
            }
        };
        match self.deleter_allowed(set, hint.forge, &who, author, &mut rep)? {
            Some(true) => {}
            Some(false) => {
                return self.hold_for_reviewer(
                    obj,
                    rep,
                    format!("{} #{} was deleted by {}, who is neither the author nor a maintainer; held for the reviewer", hint.forge.as_str(), hint.number, who.login),
                );
            }
            None => {
                return self.hold_for_reviewer(
                    obj,
                    rep,
                    format!("{} #{} was deleted by {} and the forge cannot say whether they are a maintainer; held for the reviewer", hint.forge.as_str(), hint.number, who.login),
                );
            }
        }
        if obj.kind == Kind::Contribution {
            return self.close_notice(obj, set, rep);
        }
        if let Some(why) = self.cascade_caps(&obj)? {
            return self.hold_for_reviewer(
                obj,
                rep,
                format!("cascade cap: {why}; held for the reviewer"),
            );
        }
        let now = self.clock.now();
        obj.tombstone = Some(Tombstone {
            at: now,
            reason: format!("deleted on {} by {}", hint.forge.as_str(), who.login),
        });
        let siblings = self.store.siblings_of(&obj.lid)?;
        let object = obj.clone();
        let lid = obj.lid.clone();
        let targets: Vec<Target> = siblings
            .iter()
            .filter(|s| !s.deleted)
            .filter_map(|s| {
                set.members.get(&s.forge).map(|m| Target {
                    forge: s.forge,
                    repo: m.id.clone(),
                    number: s.number,
                    comment: None,
                })
            })
            .collect();
        self.commit_cascade(&object, targets, OpKind::DeleteObject, now, &lid)?;
        let obj = self.store.object(&lid)?.unwrap_or(object);
        self.drive_deletes(&obj, set, rep)
    }

    fn cascade_comment(
        &mut self,
        obj: LogicalObject,
        set: &RepoSet,
        hint: &Hint,
        mut comment: CommentRow,
        mut rep: Report,
    ) -> Result<Report> {
        let Some(who) = hint.actor.clone() else {
            return self.hold_for_reviewer(
                obj,
                rep,
                format!("a comment on {} #{} was deleted but the forge did not say by whom; held for the reviewer", hint.forge.as_str(), hint.number),
            );
        };
        let author = (comment.origin_forge == hint.forge).then_some(&comment.author);
        match self.deleter_allowed(set, hint.forge, &who, author, &mut rep)? {
            Some(true) => {}
            _ => {
                return self.hold_for_reviewer(
                    obj,
                    rep,
                    format!("a comment on {} #{} was deleted by {}, who cannot be shown to be its author or a maintainer; held for the reviewer", hint.forge.as_str(), hint.number, who.login),
                );
            }
        }
        if let Some(why) = self.cascade_caps(&obj)? {
            return self.hold_for_reviewer(
                obj,
                rep,
                format!("cascade cap: {why}; held for the reviewer"),
            );
        }
        let now = self.clock.now();
        let hint_comment = match hint.action {
            HintAction::CommentDeleted { comment_id } => comment_id,
            _ => 0,
        };
        let siblings = self.store.siblings_of(&obj.lid)?;
        let number_on = |forge: Forge| {
            siblings
                .iter()
                .find(|s| s.forge == forge && !s.deleted)
                .map(|s| s.number)
        };
        let mut targets: Vec<Target> = Vec::new();
        let mut push = |forge: Forge, id: u64| {
            if let (Some(number), Some(member)) = (number_on(forge), set.members.get(&forge)) {
                if !(forge == hint.forge && id == hint_comment) {
                    targets.push(Target {
                        forge,
                        repo: member.id.clone(),
                        number,
                        comment: Some(id.to_string()),
                    });
                }
            }
        };
        push(comment.origin_forge, comment.origin_id);
        for (forge, copy) in &comment.copies {
            push(*forge, copy.id);
        }
        targets.sort_by_key(|t| (t.forge != Forge::Forgejo, t.forge));
        comment.tombstone = true;
        self.store.put_comment(comment)?;
        let lid = obj.lid.clone();
        self.commit_cascade(&obj, targets, OpKind::DeleteComment, now, &lid)?;
        let obj = self.store.object(&lid)?.unwrap_or(obj);
        let (done, pending) = self.drive(&obj, set, &mut rep)?;
        rep.pending = pending;
        Ok(if pending > 0 {
            rep.with(Outcome::Pending(format!(
                "{pending} comment deletion(s) still to carry out"
            )))
        } else if done > 0 {
            rep.with(Outcome::Deleted)
        } else {
            rep.with(Outcome::Converged)
        })
    }

    /// One transaction: the tombstone (objects), the cascade ledger entry, and an intent per delete.
    fn commit_cascade(
        &mut self,
        obj: &LogicalObject,
        targets: Vec<Target>,
        op: OpKind,
        now: u64,
        lid: &str,
    ) -> Result<()> {
        let mut object = obj.clone();
        if op == OpKind::DeleteObject && object.tombstone.is_none() {
            object.tombstone = Some(Tombstone {
                at: now,
                reason: "deleted".into(),
            });
        }
        let mut intents = Vec::new();
        for target in targets {
            let digest = super::model::digest(&[lid, &format!("{op:?}"), &format!("{target:?}")]);
            intents.push(super::model::Intent {
                id: uuid::Uuid::new_v4().to_string(),
                lid: lid.to_owned(),
                target,
                op,
                digest,
                created: now,
                attempts: 0,
                status: IntentStatus::Planned,
                resolved: None,
                note: None,
            });
        }
        self.store.atomically(&mut |tx| {
            if op == OpKind::DeleteObject {
                tx.put_object(object.clone())?;
            }
            tx.ledger_append(LedgerEntry {
                ts: now,
                kind: LedgerKind::Cascade,
                forge: Forge::Forgejo,
                namespace: String::new(),
                lid: Some(lid.to_owned()),
                detail: object.primary.clone(),
            })?;
            for intent in &intents {
                tx.put_intent(intent.clone())?;
            }
            Ok(())
        })
    }

    /// R52 / the maintainer: a contribution's notice is never deleted by a cascade, only closed.
    fn close_notice(
        &mut self,
        mut obj: LogicalObject,
        set: &RepoSet,
        mut rep: Report,
    ) -> Result<Report> {
        let Some(member) = set.members.get(&Forge::Forgejo) else {
            return Ok(rep.with(Outcome::Dropped("no hub".into())));
        };
        let Some(hub) = self
            .store
            .siblings_of(&obj.lid)?
            .into_iter()
            .find(|s| s.forge == Forge::Forgejo && !s.deleted)
        else {
            return Ok(rep.with(Outcome::Dropped("the notice is not on Forgejo".into())));
        };
        let ns = member.namespace().to_owned();
        let hub_ref = hub.object_ref(obj.kind);
        let target = Target {
            forge: Forge::Forgejo,
            repo: member.id.clone(),
            number: hub.number,
            comment: None,
        };
        let patch = Patch {
            state: Some(IssueState::Closed),
            ..Patch::default()
        };
        let intent = self.open_intent(
            &obj.lid,
            &target,
            OpKind::UpdateObject,
            "close: deleted at origin",
        )?;
        let sent = self.send(&obj.lid, &intent, Forge::Forgejo, &ns, &mut rep, |a| {
            a.update_object(&hub_ref, &patch)
        })?;
        if let Sent::Done { intent: id, .. } = sent {
            self.settle(&id, &obj.lid, IntentStatus::Done, None)?;
            let note_target = Target {
                comment: Some("deleted-at-origin".into()),
                ..target
            };
            let intent = self.open_intent(
                &obj.lid,
                &note_target,
                OpKind::CreateComment,
                "deleted at origin",
            )?;
            let sent = self.send(&obj.lid, &intent, Forge::Forgejo, &ns, &mut rep, |a| {
                a.create_comment(&hub_ref, "deleted at origin")
            })?;
            if let Sent::Done { intent: id, .. } = sent {
                self.settle(&id, &obj.lid, IntentStatus::Done, None)?;
            }
            obj.state = ObjState::Closed;
            self.store.put_object(obj)?;
            return Ok(rep.with(Outcome::Pending("the notice was closed with \"deleted at origin\"; it is never deleted by a cascade".into())));
        }
        Ok(rep.with(Outcome::Pending("closing the notice is pending".into())))
    }
}
