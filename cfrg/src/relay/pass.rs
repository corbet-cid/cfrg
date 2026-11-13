//! One convergence pass over a logical object (RD1, RD2, RD3, RD4).
//!
//! read every sibling -> merge their local changes into the canon -> commit the merge and the plan
//! (intents) -> write the difference, hub first -> settle each intent with the rows its write changed.
//! A sibling that already shows what it should gets no write (RD4(b)). Nothing here retries: a failed
//! write leaves its intent pending for the next hint or the sweep (RF1).

use super::engine::{Engine, Failed, Outcome, Report, Sent};
use super::forge::{NewObject, ReadOutcome, ScreenVerdict};
use super::marker::{
    credit_line, footer_markers, render_comment_body, render_list_comment, render_object_body,
    Marker, Rewrite, SibLink,
};
use super::merge::{desired, diff, merge, Input, Patch};
use super::model::{
    has_issues, lid_comment, lid_object, Actor, Canon, CommentCopy, CommentRow, Hint, HoldScope,
    IntentStatus, Kind, LedgerEntry, LedgerKind, LogicalObject, ObjState, ObjectHold, ObjectRef,
    ObjectState, OpKind, RepoSet, SiblingRow, Snap, Target,
};
use super::pacing;
use crate::{model::Forge, Result};
use std::collections::BTreeMap;

const ROUNDS: usize = 4;

pub(super) fn rank(forge: Forge) -> u8 {
    match forge {
        Forge::Forgejo => 0,
        Forge::Github => 1,
        Forge::Gitlab => 2,
        Forge::Bitbucket => 3,
    }
}

pub(super) struct Sib {
    pub row: SiblingRow,
    pub cur: Option<ObjectState>,
    pub gone: bool,
    pub blocked: Option<String>,
}

pub(super) struct View {
    pub sibs: Vec<Sib>,
    pub comments: BTreeMap<String, CommentRow>,
}

impl View {
    pub fn sib(&self, forge: Forge) -> Option<&Sib> {
        self.sibs.iter().find(|s| s.row.forge == forge)
    }

    fn sib_mut(&mut self, forge: Forge) -> Option<&mut Sib> {
        self.sibs.iter_mut().find(|s| s.row.forge == forge)
    }

    fn links(&self) -> Vec<SibLink> {
        let mut links: Vec<SibLink> = self
            .sibs
            .iter()
            .filter(|s| !s.row.deleted)
            .map(|s| SibLink {
                forge: s.row.forge,
                repo: s.row.repo.clone(),
                number: s.row.number,
                url: s.row.url.clone(),
                origin: s.row.origin,
            })
            .collect();
        links.sort_by_key(|l| rank(l.forge));
        links
    }
}

#[derive(Clone, Debug)]
enum OpData {
    Create,
    Update(Patch),
    CreateComment { lcid: String },
    UpdateComment { lcid: String, id: u64, body: String },
    CreateList { body: String },
    UpdateList { id: u64, body: String },
}

#[derive(Clone, Debug)]
struct Op {
    forge: Forge,
    data: OpData,
}

enum Step {
    Done,
    /// The forge cannot be written now; its remaining ops wait. A blocked hub ends the pass (RD2).
    Block(Forge, String),
    /// A permanent rejection of this payload; other ops go on.
    Failed(String),
}

fn origin_issue_url(origin_url: &str, number: u64) -> String {
    match origin_url.rsplit_once('/') {
        Some((base, _)) => format!("{base}/{number}"),
        None => origin_url.to_owned(),
    }
}

impl Engine<'_> {
    /// First sight of an object that has no row yet: read it, judge it, record it, run its first pass.
    pub(super) fn start_object(&mut self, hint: &Hint) -> Result<Report> {
        let mut rep = Report::new(None);
        let cfg = self.cfg;
        let Some(set) = cfg.locate(hint.forge, &hint.repo) else {
            return Ok(rep.with(Outcome::Dropped("repository is not relayed".into())));
        };
        if Self::hint_is_deletion(hint) {
            return Ok(rep.with(Outcome::Dropped(
                "a deleted object that was never tracked".into(),
            )));
        }
        let Some(member) = set.members.get(&hint.forge) else {
            return Ok(rep.with(Outcome::Dropped("repository is not relayed".into())));
        };
        if !has_issues(hint.forge) {
            return Ok(rep.with(Outcome::Dropped(format!(
                "{} has no issues",
                hint.forge.as_str()
            ))));
        }
        let object = hint.object();
        let ns = member.namespace().to_owned();
        let state = match self.read_call(hint.forge, &ns, &mut rep, |a| a.read(&object))? {
            Ok(ReadOutcome::Found(state)) => *state,
            Ok(ReadOutcome::Gone) => {
                return Ok(rep.with(Outcome::Dropped("the object does not exist".into())));
            }
            Err(Failed::Denied(d)) => return Ok(rep.with(Outcome::Deferred(d.text()))),
            Err(Failed::Error(e)) => return Ok(rep.with(Outcome::Deferred(e.to_string()))),
        };
        if state.confidential {
            return Ok(rep.with(Outcome::Dropped(
                "confidential issues are never relayed".into(),
            )));
        }
        if self.relay_authored(hint.forge, &state.author) {
            // A copy by the relay identity whose row is missing: adopt it if its markers say whose it is.
            return self.adopt_orphan(hint, &state, rep);
        }
        let lid = lid_object(&object);
        let now = self.clock.now();
        let canon = Canon {
            title: state.snap.title.clone(),
            body: state.snap.body.clone(),
            state: state.snap.state,
            labels: state
                .snap
                .labels
                .iter()
                .filter(|l| !cfg.is_local_label(l))
                .cloned()
                .collect(),
            assignees: state
                .snap
                .assignees
                .iter()
                .filter_map(|h| cfg.person_by_handle(hint.forge, h).map(|p| p.key.clone()))
                .collect(),
        };
        let row = SiblingRow {
            lid: lid.clone(),
            forge: hint.forge,
            repo: hint.repo.clone(),
            number: hint.number,
            origin: true,
            url: state.url.clone(),
            list_comment: None,
            snapshot: Some(state.snap.clone()),
            behind: None,
            deleted: false,
            last_synced: now,
        };
        let obj = LogicalObject {
            lid: lid.clone(),
            kind: Kind::Issue,
            primary: set.name.clone(),
            origin: object.clone(),
            origin_author: state.author.clone(),
            state: ObjState::Open,
            screened: false,
            held: None,
            tombstone: None,
            canon,
            created: now,
        };
        // The snapshot is empty on purpose: the first pass sees the whole origin as new content.
        let mut row = row;
        row.snapshot = None;
        self.store.atomically(&mut |tx| {
            tx.put_object(obj.clone())?;
            tx.put_sibling(row.clone())
        })?;
        let mut report = self.pass(&lid, Some(hint), Some((object, state)))?;
        report.reads += rep.reads;
        Ok(report)
    }

    fn adopt_orphan(&mut self, hint: &Hint, state: &ObjectState, rep: Report) -> Result<Report> {
        let markers = super::marker::trusted_markers(&state.snap.body, true);
        for marker in markers {
            if let Marker::Object { lid, forge } = marker {
                if forge == hint.forge {
                    if let Some(obj) = self.store.object(&lid)? {
                        let now = self.clock.now();
                        self.store.put_sibling(SiblingRow {
                            lid: obj.lid.clone(),
                            forge: hint.forge,
                            repo: hint.repo.clone(),
                            number: hint.number,
                            origin: false,
                            url: state.url.clone(),
                            list_comment: None,
                            snapshot: Some(state.snap.clone()),
                            behind: None,
                            deleted: false,
                            last_synced: now,
                        })?;
                        return self.pass(&obj.lid, None, None);
                    }
                }
            }
        }
        Ok(rep.with(Outcome::Dropped(
            "a post by the relay identity that no logical object owns".into(),
        )))
    }

    fn freeze(&mut self, obj: &mut LogicalObject, reason: &str) -> Result<()> {
        obj.held = Some(ObjectHold {
            reason: reason.to_owned(),
            scope: HoldScope::Frozen,
            since: self.clock.now(),
        });
        self.store.put_object(obj.clone())
    }

    /// One pass of one logical object. `hint` orders the merge (the named sibling is processed last)
    /// and may carry a deletion; `preread` is an object state the caller just read.
    pub(super) fn pass(
        &mut self,
        lid: &str,
        hint: Option<&Hint>,
        preread: Option<(ObjectRef, ObjectState)>,
    ) -> Result<Report> {
        let rep = Report::new(Some(lid.to_owned()));
        let cfg = self.cfg;
        let Some(mut obj) = self.store.object(lid)? else {
            return Ok(rep.with(Outcome::Dropped("unknown logical object".into())));
        };
        let Some(set) = cfg.set_named(&obj.primary) else {
            return Ok(rep.with(Outcome::Dropped(
                "the repository is no longer configured".into(),
            )));
        };
        if let Err(error) = self.refuse_contributed(set) {
            return Ok(rep.with(Outcome::Dropped(error.to_string())));
        }
        if cfg.kill_switch.as_ref().is_some_and(|p| p.exists()) {
            return Ok(rep.with(Outcome::Deferred("kill switch is set".into())));
        }
        if obj.tombstone.is_some() {
            return self.drive_deletes(&obj, set, rep);
        }
        if let Some(hold) = obj.held.as_ref().filter(|h| h.scope == HoldScope::Frozen) {
            return Ok(rep.with(Outcome::Held(hold.reason.clone())));
        }
        if let Some(h) = hint.filter(|h| Self::hint_is_deletion(h)) {
            return self.deletion(obj, set, h, rep);
        }
        if obj.kind != Kind::Issue {
            return Ok(rep.with(Outcome::Unsupported(
                "contribution flows are lanes RL4 and RL5".into(),
            )));
        }
        // RF8 backstop: a loop shows up as one writing pass per cycle.
        let now = self.clock.now();
        let passes = pacing::write_passes(&*self.store, &obj.lid, now)?;
        if passes >= cfg.limits.object_write_passes_per_hour {
            let reason = format!(
                "loop suspected: {passes} writing passes within an hour (RF8); the owner releases the object"
            );
            self.freeze(&mut obj, &reason)?;
            return Ok(rep.with(Outcome::Held(reason)));
        }
        let rep = self.drive_deletes_in_flight(&obj, set, rep)?;
        let mut rep = rep;

        let mut view = self.read_view(&obj, set, hint, preread, &mut rep)?;
        let hinted = hint.map(|h| h.forge);
        if let Some(h) = hinted {
            if let Some(why) = view.sib(h).and_then(|s| s.blocked.clone()) {
                return Ok(rep.with(Outcome::Deferred(format!(
                    "{} cannot be read: {why}",
                    h.as_str()
                ))));
            }
        }
        if view
            .sibs
            .iter()
            .any(|s| s.cur.as_ref().is_some_and(|c| c.confidential))
        {
            self.freeze(
                &mut obj,
                "the issue became confidential; it is no longer relayed (R50)",
            )?;
            return Ok(rep.with(Outcome::Held("confidential".into())));
        }
        // RD2: the hub orders every change. If it exists and cannot be read, nothing is written.
        if let Some(hub) = view.sib(Forge::Forgejo) {
            if hub.cur.is_none() && !hub.row.deleted {
                let why = hub
                    .blocked
                    .clone()
                    .unwrap_or_else(|| "the hub copy is gone".into());
                return Ok(rep.with(Outcome::Pending(format!("hub unreadable: {why}"))));
            }
        }

        // RD3: merge the local changes of every readable sibling, the hinted one last.
        let canon = {
            let mut order: Vec<&Sib> = view.sibs.iter().filter(|s| s.cur.is_some()).collect();
            order.sort_by_key(|s| (Some(s.row.forge) == hinted, rank(s.row.forge)));
            let inputs: Vec<Input<'_>> = order
                .iter()
                .filter_map(|s| {
                    s.cur.as_ref().map(|cur| Input {
                        forge: s.row.forge,
                        origin: s.row.origin,
                        cur: &cur.snap,
                        snap: s.row.snapshot.as_ref(),
                    })
                })
                .collect();
            merge(cfg, &obj.canon, &inputs)
        };
        obj.canon = canon;
        if obj.state != ObjState::TakenIn {
            obj.state = if obj.canon.state == Some(super::model::IssueState::Closed) {
                ObjState::Closed
            } else {
                ObjState::Open
            };
        }
        self.absorb_comments(&obj, &mut view, now);

        // Commit the merge: the canon, the snapshots (every readable sibling's delta is now in the canon)
        // and the comment rows. Intents of the plan follow per round, before their first request.
        {
            let object = obj.clone();
            let rows: Vec<SiblingRow> = view
                .sibs
                .iter_mut()
                .map(|s| {
                    if let Some(cur) = &s.cur {
                        s.row.snapshot = Some(cur.snap.clone());
                        s.row.last_synced = now;
                    }
                    s.row.clone()
                })
                .collect();
            let comments: Vec<CommentRow> = view.comments.values().cloned().collect();
            self.store.atomically(&mut |tx| {
                tx.put_object(object.clone())?;
                for row in &rows {
                    tx.put_sibling(row.clone())?;
                }
                for comment in &comments {
                    tx.put_comment(comment.clone())?;
                }
                Ok(())
            })?;
        }

        let mut blocked: BTreeMap<Forge, String> = BTreeMap::new();
        let mut failed: Vec<String> = Vec::new();
        'rounds: for _ in 0..ROUNDS {
            self.maybe_screen(&mut obj, &view)?;
            let ops = self.plan(&obj, set, &view, &blocked);
            let ops: Vec<Op> = ops
                .into_iter()
                .filter(|o| !blocked.contains_key(&o.forge))
                .collect();
            if ops.is_empty() {
                break;
            }
            // The plan of this round is committed before the first request (§6).
            let slots: Vec<(Target, OpKind, String)> =
                ops.iter().map(|o| self.slot(&obj, set, &view, o)).collect();
            for (target, kind, payload) in &slots {
                self.open_intent(&obj.lid, target, *kind, payload)?;
            }
            let mut progressed = false;
            for op in ops {
                if pacing::interrupted() {
                    blocked.insert(op.forge, "interrupted".into());
                    break 'rounds;
                }
                if blocked.contains_key(&op.forge) {
                    continue;
                }
                match self.exec(&obj, set, &mut view, &op, &mut rep)? {
                    Step::Done => progressed = true,
                    Step::Block(forge, why) => {
                        blocked.insert(forge, why);
                        if forge == Forge::Forgejo {
                            break 'rounds;
                        }
                    }
                    Step::Failed(why) => failed.push(why),
                }
            }
            if !progressed {
                break;
            }
        }

        // What is still to do decides the outcome; intents whose work is done are settled by observation.
        let rest: Vec<Op> = self.plan(&obj, set, &view, &BTreeMap::new());
        let rest_slots: Vec<(Target, OpKind)> = rest
            .iter()
            .map(|o| {
                let (t, k, _) = self.slot(&obj, set, &view, o);
                (t, k)
            })
            .collect();
        for intent in self.store.intents_of(&obj.lid)? {
            if intent.status == IntentStatus::Planned
                && !rest_slots.iter().any(|(t, k)| intent.same_slot(t, *k))
                && !matches!(intent.op, OpKind::DeleteObject | OpKind::DeleteComment)
            {
                self.settle(
                    &intent.id,
                    &obj.lid,
                    IntentStatus::Done,
                    Some("converged by observation".into()),
                )?;
            }
        }
        rep.pending = u32::try_from(rest.len()).unwrap_or(u32::MAX);
        let behind: BTreeMap<Forge, String> = {
            let mut m = BTreeMap::new();
            for (forge, why) in &blocked {
                m.insert(*forge, why.clone());
            }
            for op in &rest {
                m.entry(op.forge)
                    .or_insert_with(|| "changes pending".into());
            }
            for s in &view.sibs {
                if let Some(why) = &s.blocked {
                    m.entry(s.row.forge).or_insert_with(|| why.clone());
                }
                if s.gone && !s.row.deleted {
                    m.insert(s.row.forge, "gone, deletion unconfirmed".into());
                }
            }
            m
        };
        let rows: Vec<SiblingRow> = view
            .sibs
            .iter_mut()
            .map(|s| {
                s.row.behind = behind.get(&s.row.forge).cloned();
                s.row.clone()
            })
            .collect();
        let comments: Vec<CommentRow> = view.comments.values().cloned().collect();
        let object = obj.clone();
        let wrote = rep.writes > 0;
        let ts = self.clock.now();
        let lid_owned = obj.lid.clone();
        self.store.atomically(&mut |tx| {
            tx.put_object(object.clone())?;
            for row in &rows {
                tx.put_sibling(row.clone())?;
            }
            for comment in &comments {
                tx.put_comment(comment.clone())?;
            }
            if wrote {
                tx.ledger_append(LedgerEntry {
                    ts,
                    kind: LedgerKind::WritePass,
                    forge: Forge::Forgejo,
                    namespace: String::new(),
                    lid: Some(lid_owned.clone()),
                    detail: String::new(),
                })?;
            }
            Ok(())
        })?;
        for (forge, why) in &blocked {
            rep.note(format!("{} is behind: {why}", forge.as_str()));
        }
        for why in &failed {
            rep.note(format!("rejected: {why}"));
        }
        for s in &view.sibs {
            if s.gone && !s.row.deleted {
                rep.note(format!(
                    "{} #{} no longer exists; without a deletion event nothing is deleted (R50)",
                    s.row.forge.as_str(),
                    s.row.number
                ));
            }
        }
        rep.outcome = if let Some(hold) = &obj.held {
            Outcome::Held(hold.reason.clone())
        } else if rest.is_empty() {
            Outcome::Converged
        } else if let Some((forge, why)) = blocked.iter().next() {
            Outcome::Pending(format!("{}: {why}", forge.as_str()))
        } else {
            Outcome::Pending(format!("{} change(s) left", rest.len()))
        };
        Ok(rep)
    }

    /// Reads every sibling that exists (the named one may have been read by the caller already).
    fn read_view(
        &mut self,
        obj: &LogicalObject,
        set: &RepoSet,
        hint: Option<&Hint>,
        mut preread: Option<(ObjectRef, ObjectState)>,
        rep: &mut Report,
    ) -> Result<View> {
        let rows = self.store.siblings_of(&obj.lid)?;
        let mut sibs = Vec::new();
        for row in rows {
            let mut sib = Sib {
                row: row.clone(),
                cur: None,
                gone: false,
                blocked: None,
            };
            if row.deleted {
                sibs.push(sib);
                continue;
            }
            let Some(member) = set.members.get(&row.forge) else {
                sib.blocked = Some("not part of the configured set".into());
                sibs.push(sib);
                continue;
            };
            if let Some((r, state)) =
                preread.take_if(|(r, _)| r.forge == row.forge && r.number == row.number)
            {
                let _ = r;
                sib.cur = Some(state);
                sibs.push(sib);
                continue;
            }
            let object = row.object_ref(obj.kind);
            let ns = member.namespace().to_owned();
            // A deletion hint reads the named object itself in the deletion path.
            let _ = hint;
            match self.read_call(row.forge, &ns, rep, |a| a.read(&object))? {
                Ok(ReadOutcome::Found(state)) => sib.cur = Some(*state),
                Ok(ReadOutcome::Gone) => sib.gone = true,
                Err(Failed::Denied(d)) => sib.blocked = Some(d.text()),
                Err(Failed::Error(e)) => sib.blocked = Some(e.to_string()),
            }
            sibs.push(sib);
        }
        let comments = self
            .store
            .comments_of(&obj.lid)?
            .into_iter()
            .map(|c| (c.lcid.clone(), c))
            .collect();
        Ok(View { sibs, comments })
    }

    /// Reads comments into rows: human comments are origin comments (new or edited); relay comments
    /// whose trusted markers name a comment row are its copies (this also recovers a lost response).
    fn absorb_comments(&self, obj: &LogicalObject, view: &mut View, now: u64) {
        let cfg = self.cfg;
        for sib in &view.sibs {
            let Some(cur) = &sib.cur else { continue };
            for c in &cur.comments {
                if cfg.is_relay(sib.row.forge, &c.author) {
                    continue;
                }
                let lcid = lid_comment(&obj.lid, sib.row.forge, c.id);
                match view.comments.get_mut(&lcid) {
                    Some(row) => {
                        if row.origin_body != c.body {
                            row.origin_body.clone_from(&c.body);
                        }
                    }
                    None => {
                        view.comments.insert(
                            lcid.clone(),
                            CommentRow {
                                lcid,
                                lid: obj.lid.clone(),
                                origin_forge: sib.row.forge,
                                origin_id: c.id,
                                origin_url: c.url.clone(),
                                author: c.author.clone(),
                                origin_body: c.body.clone(),
                                created_at: c.created_at,
                                copies: BTreeMap::new(),
                                tombstone: false,
                            },
                        );
                    }
                }
            }
        }
        for sib in &view.sibs {
            let Some(cur) = &sib.cur else { continue };
            for c in &cur.comments {
                if !cfg.is_relay(sib.row.forge, &c.author) {
                    continue;
                }
                let markers = super::marker::trusted_markers(&c.body, true);
                for marker in &markers {
                    if let Marker::Object { lid, forge } = marker {
                        if *forge != sib.row.forge {
                            continue;
                        }
                        if view.comments.contains_key(lid) {
                            if let Some(row) = view.comments.get_mut(lid) {
                                if row.origin_forge != sib.row.forge {
                                    row.copies.insert(
                                        sib.row.forge,
                                        CommentCopy {
                                            id: c.id,
                                            body: c.body.clone(),
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        let _ = now;
        // The cforge comment that lists the siblings on a human-authored object.
        for sib in view.sibs.iter_mut().filter(|s| s.row.origin) {
            let Some(cur) = &sib.cur else { continue };
            for c in &cur.comments {
                if !cfg.is_relay(sib.row.forge, &c.author) {
                    continue;
                }
                let listed = footer_markers(&c.body).into_iter().any(|m| {
                    matches!(m, Marker::Object { lid, forge } if lid == obj.lid && forge == sib.row.forge)
                });
                if listed && sib.row.list_comment.is_none() {
                    sib.row.list_comment = Some(c.id);
                }
            }
        }
    }

    fn rewrite_parts<'c>(
        &'c self,
        obj: &LogicalObject,
        view: &'c View,
        target: Forge,
    ) -> (
        impl Fn(u64) -> Option<u64> + 'c,
        impl Fn(u64) -> String + 'c,
        Forge,
    ) {
        let store: &dyn super::store::Store = &*self.store;
        let origin = obj.origin.clone();
        let origin_url = view
            .sib(origin.forge)
            .map(|s| s.row.url.clone())
            .unwrap_or_default();
        let on_target = move |n: u64| -> Option<u64> {
            let sibling = store
                .sibling_at(origin.forge, &origin.repo, n)
                .ok()
                .flatten()?;
            let siblings = store.siblings_of(&sibling.lid).ok()?;
            siblings
                .into_iter()
                .find(|s| s.forge == target && !s.deleted)
                .map(|s| s.number)
        };
        let url = move |n: u64| origin_issue_url(&origin_url, n);
        (on_target, url, obj.origin.forge)
    }

    fn credit(&self, obj: &LogicalObject, set: &RepoSet, view: &View, target: Forge) -> String {
        let link = view
            .sib(obj.origin.forge)
            .map(|s| s.row.url.as_str())
            .unwrap_or_default();
        credit_line(
            self.cfg,
            set.credit,
            obj.origin.forge,
            target,
            &obj.origin_author,
            link,
        )
    }

    fn body_for(&self, obj: &LogicalObject, set: &RepoSet, view: &View, target: Forge) -> String {
        let (on_target, origin_url, _) = self.rewrite_parts(obj, view, target);
        let rw = Rewrite {
            cfg: self.cfg,
            source: obj.origin.forge,
            target,
            issue_on_target: &on_target,
            origin_issue_url: &origin_url,
        };
        render_object_body(
            &self.credit(obj, set, view, target),
            &obj.lid,
            target,
            &view.links(),
            &obj.canon.body,
            &rw,
        )
    }

    fn comment_body_for(
        &self,
        obj: &LogicalObject,
        set: &RepoSet,
        view: &View,
        row: &CommentRow,
        target: Forge,
    ) -> String {
        let (on_target, origin_url, _) = self.rewrite_parts(obj, view, target);
        let rw = Rewrite {
            cfg: self.cfg,
            source: row.origin_forge,
            target,
            issue_on_target: &on_target,
            origin_issue_url: &origin_url,
        };
        let line = credit_line(
            self.cfg,
            set.credit,
            row.origin_forge,
            target,
            &row.author,
            &row.origin_url,
        );
        render_comment_body(
            &line,
            &row.lcid,
            target,
            row.origin_forge,
            row.origin_id,
            &row.origin_body,
            &rw,
        )
    }

    /// R51: the abuse screen and the per-author cap, once, after the hub copy exists.
    fn maybe_screen(&mut self, obj: &mut LogicalObject, view: &View) -> Result<()> {
        if obj.screened {
            return Ok(());
        }
        if obj.origin.forge != Forge::Forgejo && view.sib(Forge::Forgejo).is_none() {
            return Ok(());
        }
        let Some(origin) = view.sib(obj.origin.forge).and_then(|s| s.cur.as_ref()) else {
            return Ok(());
        };
        let now = self.clock.now();
        let key = format!("{}:{}", obj.origin.forge.as_str(), obj.origin_author.id);
        let verdict = self.screen.screen(obj.origin.forge, origin);
        let used = pacing::author_fanouts(&*self.store, &key, now)?;
        let hold = match verdict {
            ScreenVerdict::Hold(reason) => Some(reason),
            ScreenVerdict::Pass if used >= self.cfg.limits.author_fanout_per_day => Some(format!(
                "the author already has {used} objects fanned out today (R51)"
            )),
            ScreenVerdict::Pass => None,
        };
        obj.screened = true;
        match &hold {
            Some(reason) => {
                obj.held = Some(ObjectHold {
                    reason: reason.clone(),
                    scope: HoldScope::FanOut,
                    since: now,
                });
            }
            None => obj.held = None,
        }
        let object = obj.clone();
        self.store.atomically(&mut |tx| {
            if hold.is_none() {
                tx.ledger_append(LedgerEntry {
                    ts: now,
                    kind: LedgerKind::FanOut,
                    forge: object.origin.forge,
                    namespace: String::new(),
                    lid: Some(object.lid.clone()),
                    detail: key.clone(),
                })?;
            }
            tx.put_object(object.clone())
        })
    }

    /// The difference between the canon and every sibling, hub first. Pure.
    fn plan(
        &self,
        obj: &LogicalObject,
        set: &RepoSet,
        view: &View,
        no_create: &BTreeMap<Forge, String>,
    ) -> Vec<Op> {
        let cfg = self.cfg;
        let mut ops = Vec::new();
        let held = obj.held.is_some();
        let links = view.links();
        let fanout_allowed = obj.screened && obj.held.is_none();
        for forge in [Forge::Forgejo, Forge::Github, Forge::Gitlab] {
            if forge == obj.origin.forge || !has_issues(forge) || !set.members.contains_key(&forge)
            {
                continue;
            }
            match view.sib(forge) {
                None if (forge == Forge::Forgejo || fanout_allowed)
                    && !no_create.contains_key(&forge) =>
                {
                    ops.push(Op {
                        forge,
                        data: OpData::Create,
                    })
                }
                _ => {}
            }
        }
        // Copies are created first: the bodies and sibling lists are written once, when the set is complete.
        if !ops.is_empty() {
            return ops;
        }
        let mut forges: Vec<Forge> = view.sibs.iter().map(|s| s.row.forge).collect();
        forges.sort_by_key(|f| rank(*f));
        for forge in forges {
            let Some(sib) = view.sib(forge) else { continue };
            let Some(cur) = sib.cur.as_ref().filter(|_| !sib.row.deleted) else {
                continue;
            };
            // R51: a held object lives on Forgejo only; nothing is written anywhere else.
            if held && forge != Forge::Forgejo {
                continue;
            }
            let body = (!sib.row.origin).then(|| self.body_for(obj, set, view, forge));
            let want = desired(cfg, &obj.canon, forge, &cur.snap, body.as_deref(), held);
            let patch = diff(&cur.snap, &want);
            if !patch.is_empty() {
                ops.push(Op {
                    forge,
                    data: OpData::Update(patch),
                });
            }
            if sib.row.origin && links.len() > 1 {
                let body = render_list_comment(&obj.lid, forge, &links);
                match sib.row.list_comment {
                    Some(id) => {
                        if let Some(existing) = cur.comments.iter().find(|c| c.id == id) {
                            if existing.body != body {
                                ops.push(Op {
                                    forge,
                                    data: OpData::UpdateList { id, body },
                                });
                            }
                        }
                    }
                    None => ops.push(Op {
                        forge,
                        data: OpData::CreateList { body },
                    }),
                }
            }
        }
        // Comments, in origin creation order (RF6), to every existing sibling but their own forge.
        let mut rows: Vec<&CommentRow> = view.comments.values().filter(|r| !r.tombstone).collect();
        rows.sort_by(|a, b| {
            (a.created_at, rank(a.origin_forge), a.origin_id).cmp(&(
                b.created_at,
                rank(b.origin_forge),
                b.origin_id,
            ))
        });
        let mut creates = 0usize;
        let mut targets: Vec<Forge> = view.sibs.iter().map(|s| s.row.forge).collect();
        targets.sort_by_key(|f| rank(*f));
        for forge in targets {
            let Some(sib) = view.sib(forge) else { continue };
            let Some(cur) = sib.cur.as_ref().filter(|_| !sib.row.deleted) else {
                continue;
            };
            if held && forge != Forge::Forgejo {
                continue;
            }
            for row in &rows {
                if row.origin_forge == forge {
                    continue;
                }
                let want = self.comment_body_for(obj, set, view, row, forge);
                match row.copies.get(&forge) {
                    Some(copy) => {
                        if let Some(existing) = cur.comments.iter().find(|c| c.id == copy.id) {
                            if existing.body != want {
                                ops.push(Op {
                                    forge,
                                    data: OpData::UpdateComment {
                                        lcid: row.lcid.clone(),
                                        id: copy.id,
                                        body: want,
                                    },
                                });
                            }
                        }
                    }
                    None => {
                        if creates < cfg.limits.max_new_comments_per_pass {
                            creates += 1;
                            ops.push(Op {
                                forge,
                                data: OpData::CreateComment {
                                    lcid: row.lcid.clone(),
                                },
                            });
                        }
                    }
                }
            }
        }
        ops.sort_by_key(|o| rank(o.forge));
        ops
    }

    /// The journal slot of an op: where it writes, what it is, and the payload that identifies it.
    fn slot(
        &self,
        obj: &LogicalObject,
        set: &RepoSet,
        view: &View,
        op: &Op,
    ) -> (Target, OpKind, String) {
        let member = &set.members[&op.forge];
        let number = view.sib(op.forge).map_or(0, |s| s.row.number);
        let mk = |comment: Option<String>| Target {
            forge: op.forge,
            repo: member.id.clone(),
            number,
            comment,
        };
        match &op.data {
            OpData::Create => (
                mk(None),
                OpKind::CreateObject,
                self.body_for(obj, set, view, op.forge),
            ),
            OpData::Update(p) => (mk(None), OpKind::UpdateObject, format!("{p:?}")),
            OpData::CreateComment { lcid } => {
                (mk(Some(lcid.clone())), OpKind::CreateComment, lcid.clone())
            }
            OpData::UpdateComment { id, body, .. } => (
                mk(Some(id.to_string())),
                OpKind::UpdateComment,
                body.clone(),
            ),
            OpData::CreateList { body } => {
                (mk(Some("list".into())), OpKind::CreateList, body.clone())
            }
            OpData::UpdateList { id, body } => {
                (mk(Some(id.to_string())), OpKind::UpdateList, body.clone())
            }
        }
    }

    fn exec(
        &mut self,
        obj: &LogicalObject,
        set: &RepoSet,
        view: &mut View,
        op: &Op,
        rep: &mut Report,
    ) -> Result<Step> {
        let forge = op.forge;
        let member = set.members[&forge].clone();
        let ns = member.namespace().to_owned();
        let (target, kind, payload) = self.slot(obj, set, view, op);
        let intent = self.open_intent(&obj.lid, &target, kind, &payload)?;
        let object = ObjectRef {
            forge,
            repo: member.id.clone(),
            kind: obj.kind,
            number: target.number,
        };
        let now = self.clock.now();
        let relay_actor = Actor {
            id: self
                .cfg
                .relay
                .get(&forge)
                .and_then(|v| v.first().cloned())
                .unwrap_or_default(),
            login: "corbet-forge".into(),
        };
        let step = |sent: Sent<()>| -> Step {
            match sent {
                Sent::Done { .. } => Step::Done,
                Sent::Skipped(t) | Sent::Unavailable(t) | Sent::Held(t) => Step::Block(forge, t),
                Sent::Rejected(t) => Step::Failed(t),
            }
        };
        match &op.data {
            OpData::Create => {
                let want = desired(
                    self.cfg,
                    &obj.canon,
                    forge,
                    &Snap::default(),
                    Some(&payload),
                    obj.held.is_some(),
                );
                let new = NewObject {
                    title: want.title.clone(),
                    body: payload.clone(),
                    labels: want.labels.iter().cloned().collect(),
                };
                if intent.attempts > 0 {
                    // An earlier attempt may have worked with its answer lost: look for our marker first.
                    let marker = Marker::Object {
                        lid: obj.lid.clone(),
                        forge,
                    }
                    .render();
                    match self
                        .read_call(forge, &ns, rep, |a| a.find_by_marker(&member.id, &marker))?
                    {
                        Ok(Some(found)) => {
                            let row = SiblingRow {
                                lid: obj.lid.clone(),
                                forge,
                                repo: member.id.clone(),
                                number: found.number,
                                origin: false,
                                url: found.url.clone(),
                                list_comment: None,
                                snapshot: Some(found.snap.clone()),
                                behind: None,
                                deleted: false,
                                last_synced: now,
                            };
                            self.settle_with_sibling(&obj.lid, &intent.id, &row)?;
                            view.sibs.push(Sib {
                                cur: Some(fresh_state(&found, relay_actor)),
                                row,
                                gone: false,
                                blocked: None,
                            });
                            return Ok(Step::Done);
                        }
                        Ok(None) => {}
                        Err(Failed::Denied(d)) => return Ok(Step::Block(forge, d.text())),
                        Err(Failed::Error(e)) => return Ok(Step::Block(forge, e.to_string())),
                    }
                }
                match self.send(&obj.lid, &intent, forge, &ns, rep, |a| {
                    a.create_object(&member.id, &new)
                })? {
                    Sent::Done { intent: id, value } => {
                        let row = SiblingRow {
                            lid: obj.lid.clone(),
                            forge,
                            repo: member.id.clone(),
                            number: value.number,
                            origin: false,
                            url: value.url.clone(),
                            list_comment: None,
                            snapshot: Some(value.snap.clone()),
                            behind: None,
                            deleted: false,
                            last_synced: now,
                        };
                        self.settle_with_sibling(&obj.lid, &id, &row)?;
                        view.sibs.push(Sib {
                            cur: Some(fresh_state(&value, relay_actor)),
                            row,
                            gone: false,
                            blocked: None,
                        });
                        Ok(Step::Done)
                    }
                    Sent::Skipped(t) | Sent::Unavailable(t) | Sent::Held(t) => {
                        Ok(Step::Block(forge, t))
                    }
                    Sent::Rejected(t) => Ok(Step::Failed(t)),
                }
            }
            OpData::Update(patch) => {
                let sent = self.send(&obj.lid, &intent, forge, &ns, rep, |a| {
                    a.update_object(&object, patch)
                })?;
                if let Sent::Done { intent: id, .. } = &sent {
                    if let Some(sib) = view.sib_mut(forge) {
                        if let Some(cur) = sib.cur.as_mut() {
                            patch.apply(&mut cur.snap);
                            sib.row.snapshot = Some(cur.snap.clone());
                        }
                        let row = sib.row.clone();
                        self.settle_with_sibling(&obj.lid, id, &row)?;
                    }
                }
                Ok(step(sent.map_unit()))
            }
            OpData::CreateComment { lcid } => {
                let Some(row) = view.comments.get(lcid).cloned() else {
                    return Ok(Step::Done);
                };
                let body = self.comment_body_for(obj, set, view, &row, forge);
                let sent = self.send(&obj.lid, &intent, forge, &ns, rep, |a| {
                    a.create_comment(&object, &body)
                })?;
                match sent {
                    Sent::Done { intent: id, value } => {
                        let mut row = row;
                        row.copies.insert(
                            forge,
                            CommentCopy {
                                id: value,
                                body: body.clone(),
                            },
                        );
                        self.settle_with_comment(&obj.lid, &id, &row)?;
                        if let Some(cur) = view.sib_mut(forge).and_then(|s| s.cur.as_mut()) {
                            cur.comments.push(super::model::CommentState {
                                id: value,
                                url: String::new(),
                                body,
                                author: relay_actor,
                                created_at: now,
                            });
                        }
                        view.comments.insert(row.lcid.clone(), row);
                        Ok(Step::Done)
                    }
                    other => Ok(step(other.map_unit())),
                }
            }
            OpData::UpdateComment { lcid, id, body } => {
                let sent = self.send(&obj.lid, &intent, forge, &ns, rep, |a| {
                    a.update_comment(&object, *id, body)
                })?;
                if let Sent::Done { intent: iid, .. } = &sent {
                    if let Some(mut row) = view.comments.get(lcid).cloned() {
                        row.copies.insert(
                            forge,
                            CommentCopy {
                                id: *id,
                                body: body.clone(),
                            },
                        );
                        self.settle_with_comment(&obj.lid, iid, &row)?;
                        view.comments.insert(row.lcid.clone(), row);
                    }
                    if let Some(cur) = view.sib_mut(forge).and_then(|s| s.cur.as_mut()) {
                        if let Some(c) = cur.comments.iter_mut().find(|c| c.id == *id) {
                            c.body.clone_from(body);
                        }
                    }
                }
                Ok(step(sent.map_unit()))
            }
            OpData::CreateList { body } => {
                let sent = self.send(&obj.lid, &intent, forge, &ns, rep, |a| {
                    a.create_comment(&object, body)
                })?;
                match sent {
                    Sent::Done { intent: id, value } => {
                        if let Some(sib) = view.sib_mut(forge) {
                            sib.row.list_comment = Some(value);
                            if let Some(cur) = sib.cur.as_mut() {
                                cur.comments.push(super::model::CommentState {
                                    id: value,
                                    url: String::new(),
                                    body: body.clone(),
                                    author: relay_actor,
                                    created_at: now,
                                });
                            }
                            let row = sib.row.clone();
                            self.settle_with_sibling(&obj.lid, &id, &row)?;
                        }
                        Ok(Step::Done)
                    }
                    other => Ok(step(other.map_unit())),
                }
            }
            OpData::UpdateList { id, body } => {
                let sent = self.send(&obj.lid, &intent, forge, &ns, rep, |a| {
                    a.update_comment(&object, *id, body)
                })?;
                if let Sent::Done { intent: iid, .. } = &sent {
                    self.settle(iid, &obj.lid, IntentStatus::Done, None)?;
                    if let Some(cur) = view.sib_mut(forge).and_then(|s| s.cur.as_mut()) {
                        if let Some(c) = cur.comments.iter_mut().find(|c| c.id == *id) {
                            c.body.clone_from(body);
                        }
                    }
                }
                Ok(step(sent.map_unit()))
            }
        }
    }

    fn settle_with_sibling(&mut self, lid: &str, intent: &str, row: &SiblingRow) -> Result<()> {
        let now = self.clock.now();
        let row = row.clone();
        self.store.atomically(&mut |tx| {
            tx.put_sibling(row.clone())?;
            if let Some(mut i) = tx.intents_of(lid)?.into_iter().find(|i| i.id == intent) {
                i.status = IntentStatus::Done;
                i.resolved = Some(now);
                tx.put_intent(i)?;
            }
            Ok(())
        })
    }

    fn settle_with_comment(&mut self, lid: &str, intent: &str, row: &CommentRow) -> Result<()> {
        let now = self.clock.now();
        let row = row.clone();
        self.store.atomically(&mut |tx| {
            tx.put_comment(row.clone())?;
            if let Some(mut i) = tx.intents_of(lid)?.into_iter().find(|i| i.id == intent) {
                i.status = IntentStatus::Done;
                i.resolved = Some(now);
                tx.put_intent(i)?;
            }
            Ok(())
        })
    }
}

fn fresh_state(created: &super::forge::Created, author: Actor) -> ObjectState {
    ObjectState {
        number: created.number,
        url: created.url.clone(),
        author,
        snap: created.snap.clone(),
        confidential: false,
        comments: Vec::new(),
        updated_at: 0,
    }
}

impl Sent<()> {}

impl<T> Sent<T> {
    fn map_unit(self) -> Sent<()> {
        match self {
            Self::Done { intent, .. } => Sent::Done { intent, value: () },
            Self::Skipped(t) => Sent::Skipped(t),
            Self::Unavailable(t) => Sent::Unavailable(t),
            Self::Rejected(t) => Sent::Rejected(t),
            Self::Held(t) => Sent::Held(t),
        }
    }
}
