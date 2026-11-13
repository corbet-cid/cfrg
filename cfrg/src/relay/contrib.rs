//! The contribution flows (RL4, RL5; Bridge design §3.3; operating model R36-R39, R50-R53).
//!
//! A pull or merge request event on any forge is a hint (RD1). The engine reads the request, then, under the
//! lease of its logical object and behind the same guards as the issue relay:
//!
//! 1. R51: the abuse screen and the per-author cap decide once whether the request leaves Forgejo. A held
//!    contribution keeps its notice (the reviewer needs it) and gets no sibling.
//! 2. R41: every sibling request carries the credit header of its target forge.
//! 3. Heads and notice ([`super::notice::sync_contribution`]): a credited sibling on every other copy forge
//!    (Bitbucket has none, R55), one `contribution` notice on the Forgejo primary, the heads and the notice marker
//!    moved on new commits. A request closed at its origin retires the siblings and closes the notice; take-in (or
//!    a merge seen at the origin) closes everything as merged.
//! 4. Comments on any of the requests are copied to the others like issue comments (append-only; the target's own
//!    relay comments carry the same trusted marker, so a repeat finds them and writes nothing).
//!
//! Nothing is stored beyond one object row (state, screen result, a fingerprint of what the last complete pass
//! converged to) and the origin sibling row: heads and notice are pure functions of the origin and are read live
//! (RD10). A repeated event with an unchanged fingerprint writes nothing. A deletion is never carried: a request
//! that disappeared is dropped, its siblings stay (R50).

use super::engine::{Engine, Failed, Outcome, Report};
use super::forge::{ForgeError, RequestRead, ScreenVerdict};
use super::heads::{Copy, CopyReport, HeadAction, Heads, Origin, ReqState};
use super::marker::{credit_line, render_comment_body, rewrite_text, Marker, Rewrite};
use super::model::{
    digest, lid_comment, lid_object, Canon, Hint, HintAction, HoldScope, Kind, LedgerEntry,
    LedgerKind, LogicalObject, ObjState, ObjectHold, ObjectRef, ObjectState, RepoSet, SiblingRow,
    Snap,
};
use super::notice::{self, close_notice, NoticeAction, NoticeHost, NoticeTarget};
use super::pacing;
use crate::{model::Forge, Result};

/// The write ports of the contribution flow: heads on the copies and the notice on the primary.
pub struct Contrib {
    pub heads: Heads,
    pub notices: Box<dyn NoticeHost>,
}

/// Forges that can show a relay head (Bitbucket cannot, R55; the Forgejo primary shows the notice instead).
const COPY_FORGES: [Forge; 2] = [Forge::Github, Forge::Gitlab];

fn origin_request_url(request_url: &str, number: u64) -> String {
    match request_url.rsplit_once('/') {
        Some((base, _)) => format!("{base}/{number}"),
        None => request_url.to_owned(),
    }
}

/// `cforge/<forge>-<n>` back to the origin it names.
fn parse_relay_branch(branch: &str) -> Option<(Forge, u64)> {
    let rest = branch.strip_prefix("cforge/")?;
    let (forge, number) = rest.rsplit_once('-')?;
    Some((Forge::parse(forge).ok()?, number.parse().ok()?))
}

struct Subject {
    forge: Forge,
    repo_id: String,
    request: RequestRead,
}

impl Engine<'_> {
    /// One pull or merge request event (not a deletion: those take the common deletion path).
    pub(super) fn apply_contribution(&mut self, hint: &Hint) -> Result<Report> {
        let mut rep = Report::new(None);
        let Some(contrib) = self.contrib.take() else {
            return Ok(rep.with(Outcome::Unsupported(
                "contribution flows are not attached to this engine".into(),
            )));
        };
        let result = self.contribution_event(hint, contrib, None, &mut rep);
        self.contrib = Some(contrib);
        result
    }

    /// Take-in: the owner pushed `reviewed_sha`; every sibling is closed as merged, the relay heads go and the notice
    /// is closed. Idempotent. `hint` names the request at its origin.
    pub fn take_in_contribution(&mut self, hint: &Hint, reviewed_sha: &str) -> Result<Report> {
        let mut rep = Report::new(None);
        if let Err(reason) = self.admit_for_contribution(hint) {
            return Ok(rep.with(Outcome::Dropped(reason)));
        }
        let Some(contrib) = self.contrib.take() else {
            return Ok(rep.with(Outcome::Unsupported(
                "contribution flows are not attached to this engine".into(),
            )));
        };
        let result = self.contribution_event(hint, contrib, Some(reviewed_sha), &mut rep);
        self.contrib = Some(contrib);
        result
    }

    fn admit_for_contribution(&self, hint: &Hint) -> std::result::Result<(), String> {
        let Some(set) = self.cfg.locate(hint.forge, &hint.repo) else {
            return Err("repository is not relayed".into());
        };
        self.refuse_contributed(set).map_err(|e| e.to_string())
    }

    fn contribution_event(
        &mut self,
        hint: &Hint,
        contrib: &mut Contrib,
        take_in: Option<&str>,
        rep: &mut Report,
    ) -> Result<Report> {
        let cfg = self.cfg;
        let Some(set) = cfg.locate(hint.forge, &hint.repo) else {
            return Ok(rep
                .clone()
                .with(Outcome::Dropped("repository is not relayed".into())));
        };
        if hint.forge == Forge::Bitbucket {
            return Ok(rep.clone().with(Outcome::Dropped(
                "Bitbucket has no relay heads (R55)".into(),
            )));
        }
        let subject = match self.resolve_subject(hint, set, rep)? {
            Ok(s) => s,
            Err(outcome) => return Ok(std::mem::replace(rep, Report::new(None)).with(outcome)),
        };
        let object = ObjectRef {
            forge: subject.forge,
            repo: subject.repo_id.clone(),
            kind: Kind::Contribution,
            number: subject.request.number,
        };
        let lid = lid_object(&object);
        let mut report = self.led(&lid, |engine| {
            let mut rep = Report::new(Some(lid.clone()));
            engine.contribution_pass(
                &lid,
                set,
                &subject,
                hint.action == HintAction::Reconcile,
                take_in,
                contrib,
                &mut rep,
            )
        })?;
        report.reads += rep.reads;
        Ok(report)
    }

    /// Reads the request the hint names; a request the relay opened itself leads to its origin.
    fn resolve_subject(
        &mut self,
        hint: &Hint,
        set: &RepoSet,
        rep: &mut Report,
    ) -> Result<std::result::Result<Subject, Outcome>> {
        let mut forge = hint.forge;
        let mut repo_id = hint.repo.clone();
        let mut number = hint.number;
        for _ in 0..2 {
            let Some(member) = set.members.get(&forge) else {
                return Ok(Err(Outcome::Dropped("repository is not relayed".into())));
            };
            let ns = member.namespace().to_owned();
            let id = repo_id.clone();
            let read = self.read_call(forge, &ns, rep, |a| a.read_request(&id, number))?;
            let request = match read {
                Ok(Some(request)) => request,
                Ok(None) => {
                    return Ok(Err(Outcome::Dropped(
                        "the request does not exist; a deletion is never carried (R50)".into(),
                    )))
                }
                Err(Failed::Denied(d)) => return Ok(Err(Outcome::Deferred(d.text()))),
                Err(Failed::Error(e)) => return Ok(Err(Outcome::Deferred(e.to_string()))),
            };
            if !self.relay_authored(forge, &request.author) {
                return Ok(Ok(Subject {
                    forge,
                    repo_id,
                    request,
                }));
            }
            // RD4(a): the relay's own request is never an origin; its head names the origin it shows.
            let Some((origin_forge, origin_number)) = parse_relay_branch(&request.head_branch)
            else {
                return Ok(Err(Outcome::Dropped(
                    "a request by the relay identity that is not a relay head".into(),
                )));
            };
            let Some(origin_member) = set.members.get(&origin_forge) else {
                return Ok(Err(Outcome::Dropped(
                    "the relay head names a forge outside the set".into(),
                )));
            };
            forge = origin_forge;
            repo_id = origin_member.id.clone();
            number = origin_number;
        }
        Ok(Err(Outcome::Dropped(
            "the relay head leads to another relay request".into(),
        )))
    }

    #[allow(clippy::too_many_arguments)]
    fn contribution_pass(
        &mut self,
        lid: &str,
        set: &RepoSet,
        subject: &Subject,
        force: bool,
        take_in: Option<&str>,
        contrib: &mut Contrib,
        rep: &mut Report,
    ) -> Result<Report> {
        let cfg = self.cfg;
        let now = self.clock.now();
        let req = &subject.request;
        let origin_ref = ObjectRef {
            forge: subject.forge,
            repo: subject.repo_id.clone(),
            kind: Kind::Contribution,
            number: req.number,
        };
        let mut obj = match self.store.object(lid)? {
            Some(o) => o,
            None => LogicalObject {
                lid: lid.to_owned(),
                kind: Kind::Contribution,
                primary: set.name.clone(),
                origin: origin_ref,
                origin_author: req.author.clone(),
                state: ObjState::Open,
                screened: false,
                held: None,
                tombstone: None,
                canon: Canon::default(),
                created: now,
            },
        };
        let first_sight = self.store.object(lid)?.is_none();
        if let Some(hold) = obj.held.as_ref().filter(|h| h.scope == HoldScope::Frozen) {
            return Ok(rep.clone().with(Outcome::Held(hold.reason.clone())));
        }
        let passes = pacing::write_passes(&*self.store, &lid.to_owned(), now)?;
        if passes >= cfg.limits.object_write_passes_per_hour {
            let reason = format!(
                "loop suspected: {passes} writing passes within an hour (RF8); the owner releases the object"
            );
            obj.held = Some(ObjectHold {
                reason: reason.clone(),
                scope: HoldScope::Frozen,
                since: now,
            });
            self.store.put_object(obj)?;
            return Ok(rep.clone().with(Outcome::Held(reason)));
        }
        // A request that was never open and is not open now has nothing to show.
        if first_sight && req.state != ReqState::Open {
            return Ok(rep.clone().with(Outcome::Dropped(
                "a request that was never seen open".into(),
            )));
        }
        if obj.state == ObjState::TakenIn && take_in.is_none() {
            return Ok(rep.clone().with(Outcome::Converged));
        }

        let primary = set
            .members
            .get(&Forge::Forgejo)
            .map(|m| m.path.clone())
            .unwrap_or_else(|| set.name.clone());
        let link = req.url.clone();
        let rewrite = |target: Forge| {
            let url = link.clone();
            let issue_url = move |n: u64| origin_request_url(&url, n);
            let none = |_: u64| None;
            let rw = Rewrite {
                cfg,
                source: subject.forge,
                target,
                issue_on_target: &none,
                origin_issue_url: &issue_url,
            };
            let credit = credit_line(cfg, set.credit, subject.forge, target, &req.author, &link);
            format!(
                "{credit}\n\n---\n\n{}\n\n{}\n",
                rewrite_text(&req.body, &rw),
                Marker::Object {
                    lid: lid.to_owned(),
                    forge: target
                }
                .render()
            )
        };
        let mut copies: Vec<Copy> = Vec::new();
        let mut bodies = std::collections::BTreeMap::new();
        for forge in COPY_FORGES {
            let Some(member) = set.members.get(&forge) else {
                continue;
            };
            if forge == subject.forge {
                continue;
            }
            copies.push(Copy {
                forge,
                repo: member.path.clone(),
                base: req.base.clone(),
            });
            bodies.insert(forge, rewrite(forge));
        }
        let origin_member = &set.members[&subject.forge];
        let origin = Origin {
            forge: subject.forge,
            repo: origin_member.path.clone(),
            number: req.number,
            head_sha: req.head_sha.clone(),
            fetch_url: req.fetch_url.clone(),
            reference: req.reference.clone(),
            title: req.title.clone(),
            body: rewrite(Forge::Forgejo),
            bodies,
            state: if take_in.is_some() {
                ReqState::Merged
            } else {
                req.state
            },
        };
        let target = NoticeTarget {
            primary: &primary,
            origin_url: &req.fetch_url,
            reference: &req.reference,
        };

        // R51, once, on the first open sight (and again after the owner released the object).
        if !obj.screened && req.state == ReqState::Open && take_in.is_none() {
            self.screen_contribution(&mut obj, req, now)?;
        }
        let held = obj.held.is_some();
        let mut pending: Vec<String> = Vec::new();
        let mut written = 0u32;

        // Take-in (the owner's merge, or a merge seen at the origin): everything closes as merged.
        let merged_here = take_in.is_some() || req.state == ReqState::Merged;
        let fingerprint = digest(&[
            &format!("{:?}", origin.state),
            &req.head_sha,
            &req.title,
            &req.body,
            &held.to_string(),
        ]);
        let dirty = force || obj.canon.body != fingerprint;

        if merged_here {
            if obj.state != ObjState::TakenIn || force || take_in.is_some() {
                let sha = take_in.unwrap_or(&req.head_sha).to_owned();
                let usable = self.usable_copies(&copies, &mut pending)?;
                let forgejo_ok = self.forge_writable(Forge::Forgejo, set, &mut pending)?;
                let merged_origin = Origin {
                    state: ReqState::Merged,
                    ..origin.clone()
                };
                let reports = contrib.heads.take_in(&merged_origin, &usable, &sha);
                written += self.account_reports(&reports, set, &mut pending)?;
                if forgejo_ok {
                    let closed = close_notice(
                        &mut *contrib.notices,
                        &target,
                        &format!("Merged on the primary forge at {sha}."),
                    );
                    written += self.account_notice(closed, set, &mut pending)?;
                }
                obj.state = ObjState::TakenIn;
                if pending.is_empty() {
                    obj.canon.body = fingerprint;
                }
            }
        } else if dirty {
            let usable = if held {
                Vec::new()
            } else {
                self.usable_copies(&copies, &mut pending)?
            };
            let forgejo_ok = self.forge_writable(Forge::Forgejo, set, &mut pending)?;
            for c in &usable {
                // One slot per copy forge before its writes (the gap is kept across jobs by the store).
                let ns = set.members[&c.forge].namespace().to_owned();
                pacing::pace_write(&mut *self.store, cfg, self.clock, c.forge, &ns, true)?;
            }
            if forgejo_ok {
                let ns = set.members[&Forge::Forgejo].namespace().to_owned();
                pacing::pace_write(&mut *self.store, cfg, self.clock, Forge::Forgejo, &ns, true)?;
            }
            let (reports, notice) = if forgejo_ok {
                notice::sync_contribution(
                    &mut contrib.heads,
                    &mut *contrib.notices,
                    &origin,
                    &usable,
                    &target,
                )
            } else {
                (contrib.heads.sync(&origin, &usable), Ok(None))
            };
            written += self.account_reports(&reports, set, &mut pending)?;
            written += self.account_notice(notice, set, &mut pending)?;
            if let Some(sib) = self.notice_row(lid, contrib, set, &target, subject, now) {
                self.store.put_sibling(sib)?;
            }
            obj.state = match req.state {
                ReqState::Open => ObjState::Open,
                _ => ObjState::Closed,
            };
            if pending.is_empty() {
                obj.canon.body = fingerprint;
            }
        }

        // Comments between the requests (append-only, like issue comments).
        if !held && !merged_here && req.state == ReqState::Open {
            written += self.relay_request_comments(
                lid,
                set,
                subject,
                &origin,
                &copies,
                contrib,
                &mut pending,
                rep,
            )?;
        }

        let row = SiblingRow {
            lid: lid.to_owned(),
            forge: subject.forge,
            repo: subject.repo_id.clone(),
            number: req.number,
            origin: true,
            url: req.url.clone(),
            list_comment: None,
            snapshot: Some(Snap {
                title: req.title.clone(),
                body: req.body.clone(),
                ..Snap::default()
            }),
            behind: (!pending.is_empty()).then(|| pending.join("; ")),
            deleted: false,
            last_synced: now,
        };
        rep.writes += written;
        let ts = self.clock.now();
        let object = obj.clone();
        let lid_owned = lid.to_owned();
        self.store.atomically(&mut |tx| {
            tx.put_object(object.clone())?;
            tx.put_sibling(row.clone())?;
            if written > 0 {
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
        rep.pending = u32::try_from(pending.len()).unwrap_or(u32::MAX);
        for why in &pending {
            rep.note(why.clone());
        }
        let outcome = if let Some(hold) = &obj.held {
            Outcome::Held(hold.reason.clone())
        } else if pending.is_empty() {
            Outcome::Converged
        } else {
            Outcome::Pending(pending.join("; "))
        };
        Ok(rep.clone().with(outcome))
    }

    /// R51: the abuse screen and the per-author cap, once.
    fn screen_contribution(
        &mut self,
        obj: &mut LogicalObject,
        req: &RequestRead,
        now: u64,
    ) -> Result<()> {
        let state = ObjectState {
            number: req.number,
            url: req.url.clone(),
            author: req.author.clone(),
            snap: Snap {
                title: req.title.clone(),
                body: req.body.clone(),
                ..Snap::default()
            },
            confidential: false,
            comments: req.comments.clone(),
            updated_at: req.updated_at,
        };
        let key = format!("{}:{}", obj.origin.forge.as_str(), obj.origin_author.id);
        let verdict = self.screen.screen(obj.origin.forge, &state);
        let used = pacing::author_fanouts(&*self.store, &key, now)?;
        let hold = match verdict {
            ScreenVerdict::Hold(reason) => Some(reason),
            ScreenVerdict::Pass if used >= self.cfg.limits.author_fanout_per_day => Some(format!(
                "the author already has {used} objects fanned out today (R51)"
            )),
            ScreenVerdict::Pass => None,
        };
        obj.screened = true;
        obj.held = hold.map(|reason| ObjectHold {
            reason,
            scope: HoldScope::FanOut,
            since: now,
        });
        if obj.held.is_none() {
            self.store.ledger_append(LedgerEntry {
                ts: now,
                kind: LedgerKind::FanOut,
                forge: obj.origin.forge,
                namespace: String::new(),
                lid: Some(obj.lid.clone()),
                detail: key,
            })?;
        }
        Ok(())
    }

    /// Copies whose namespace may be written now (kill switch, hold, caps); the rest wait as pending.
    fn usable_copies(&mut self, copies: &[Copy], pending: &mut Vec<String>) -> Result<Vec<Copy>> {
        let mut out = Vec::new();
        for c in copies {
            let Some(set) = self.cfg.repos.iter().find_map(|s| {
                s.members
                    .get(&c.forge)
                    .filter(|m| m.path == c.repo)
                    .map(|_| s)
            }) else {
                continue;
            };
            if self.forge_writable(c.forge, set, pending)? {
                out.push(c.clone());
            }
        }
        Ok(out)
    }

    fn forge_writable(
        &mut self,
        forge: Forge,
        set: &RepoSet,
        pending: &mut Vec<String>,
    ) -> Result<bool> {
        let Some(member) = set.members.get(&forge) else {
            return Ok(false);
        };
        let now = self.clock.now();
        match pacing::admit(&*self.store, self.cfg, forge, member.namespace(), true, now)? {
            Ok(()) => Ok(true),
            Err(denied) => {
                pending.push(format!("{}: {}", forge.as_str(), denied.text()));
                Ok(false)
            }
        }
    }

    /// Ledger entries for the writes a head report stands for; refusals hold the namespace (RD11); returns the writes.
    fn account_reports(
        &mut self,
        reports: &[CopyReport],
        set: &RepoSet,
        pending: &mut Vec<String>,
    ) -> Result<u32> {
        let mut writes = 0;
        for r in reports {
            let ns = set
                .members
                .get(&r.forge)
                .map_or_else(String::new, |m| m.namespace().to_owned());
            let n = match &r.result {
                Ok(HeadAction::Created(_) | HeadAction::Recreated(_)) => 2,
                Ok(HeadAction::Updated { .. }) => 1,
                Ok(HeadAction::Retired) => 2,
                Ok(HeadAction::ClosedLeftover) => 2,
                Ok(_) => 0,
                Err(e) => {
                    self.after_error(r.forge, &ns, e)?;
                    pending.push(format!("{} {}: {e}", r.forge.as_str(), r.repo));
                    0
                }
            };
            for _ in 0..n {
                self.ledger_write(r.forge, &ns)?;
            }
            writes += n;
        }
        Ok(writes)
    }

    fn account_notice(
        &mut self,
        notice: std::result::Result<Option<NoticeAction>, ForgeError>,
        set: &RepoSet,
        pending: &mut Vec<String>,
    ) -> Result<u32> {
        let ns = set.members[&Forge::Forgejo].namespace().to_owned();
        match notice {
            Ok(Some(
                NoticeAction::Created(_) | NoticeAction::Updated(_) | NoticeAction::Closed(_),
            )) => {
                self.ledger_write(Forge::Forgejo, &ns)?;
                Ok(1)
            }
            Ok(_) => Ok(0),
            Err(e) => {
                self.after_error(Forge::Forgejo, &ns, &e)?;
                pending.push(format!("forgejo notice: {e}"));
                Ok(0)
            }
        }
    }

    fn ledger_write(&mut self, forge: Forge, ns: &str) -> Result<()> {
        let ts = self.clock.now();
        self.store.ledger_append(LedgerEntry {
            ts,
            kind: LedgerKind::Write,
            forge,
            namespace: ns.to_owned(),
            lid: None,
            detail: String::new(),
        })
    }

    /// The Forgejo notice as a sibling row, so a later deletion of the origin request closes it (R52).
    fn notice_row(
        &mut self,
        lid: &str,
        contrib: &mut Contrib,
        set: &RepoSet,
        target: &NoticeTarget<'_>,
        subject: &Subject,
        now: u64,
    ) -> Option<SiblingRow> {
        if subject.forge == Forge::Forgejo {
            return None;
        }
        let found = contrib
            .notices
            .find(target.primary, target.origin_url, target.reference)
            .ok()??;
        Some(SiblingRow {
            lid: lid.to_owned(),
            forge: Forge::Forgejo,
            repo: set.members.get(&Forge::Forgejo)?.id.clone(),
            number: found.number,
            origin: false,
            url: String::new(),
            list_comment: None,
            snapshot: None,
            behind: None,
            deleted: false,
            last_synced: now,
        })
    }

    /// Copies comments of the origin request and of every sibling request to the others, once each.
    #[allow(clippy::too_many_arguments)]
    fn relay_request_comments(
        &mut self,
        lid: &str,
        set: &RepoSet,
        subject: &Subject,
        origin: &Origin,
        copies: &[Copy],
        contrib: &mut Contrib,
        pending: &mut Vec<String>,
        rep: &mut Report,
    ) -> Result<u32> {
        let cfg = self.cfg;
        // Every request that shows this contribution: (forge, repo id, number, url, comments).
        struct Showing {
            forge: Forge,
            repo_id: String,
            number: u64,
            comments: Vec<super::model::CommentState>,
        }
        let mut showing = vec![Showing {
            forge: subject.forge,
            repo_id: subject.repo_id.clone(),
            number: subject.request.number,
            comments: subject.request.comments.clone(),
        }];
        for c in copies {
            let sibling = match contrib.heads.sibling(origin, c) {
                Ok(Some(r)) if r.state == ReqState::Open => r,
                Ok(_) => continue,
                Err(e) => {
                    pending.push(format!("{} sibling: {e}", c.forge.as_str()));
                    continue;
                }
            };
            let member = &set.members[&c.forge];
            let ns = member.namespace().to_owned();
            let id = member.id.clone();
            let number = sibling.number;
            match self.read_call(c.forge, &ns, rep, |a| a.read_request(&id, number))? {
                Ok(Some(r)) => showing.push(Showing {
                    forge: c.forge,
                    repo_id: id,
                    number,
                    comments: r.comments,
                }),
                Ok(None) => {}
                Err(Failed::Denied(d)) => {
                    pending.push(format!("{}: {}", c.forge.as_str(), d.text()))
                }
                Err(Failed::Error(e)) => pending.push(format!("{}: {e}", c.forge.as_str())),
            }
        }
        let mut written = 0;
        let mut created = 0usize;
        // Human comments, oldest first across all requests.
        let mut human: Vec<(u64, Forge, super::model::CommentState)> = Vec::new();
        for s in &showing {
            for c in &s.comments {
                if !cfg.is_relay(s.forge, &c.author) {
                    human.push((c.created_at, s.forge, c.clone()));
                }
            }
        }
        human.sort_by_key(|(t, f, c)| (*t, super::pass::rank(*f), c.id));
        for (_, from, comment) in human {
            let lcid = lid_comment(lid, from, comment.id);
            for dest in &showing {
                if dest.forge == from {
                    continue;
                }
                let have = dest.comments.iter().any(|c| {
                    cfg.is_relay(dest.forge, &c.author)
                        && super::marker::trusted_markers(&c.body, true).iter().any(|m| {
                            matches!(m, Marker::Object { lid: l, forge } if *l == lcid && *forge == dest.forge)
                        })
                });
                if have {
                    continue;
                }
                if created >= cfg.limits.max_new_comments_per_pass {
                    pending.push("more comments than one pass relays".into());
                    return Ok(written);
                }
                let member = &set.members[&dest.forge];
                let ns = member.namespace().to_owned();
                let now = self.clock.now();
                if let Err(denied) = pacing::admit(&*self.store, cfg, dest.forge, &ns, true, now)? {
                    pending.push(format!("{}: {}", dest.forge.as_str(), denied.text()));
                    continue;
                }
                let line = credit_line(
                    cfg,
                    set.credit,
                    from,
                    dest.forge,
                    &comment.author,
                    &comment.url,
                );
                let link = subject.request.url.clone();
                let issue_url = move |n: u64| origin_request_url(&link, n);
                let none = |_: u64| None;
                let rw = Rewrite {
                    cfg,
                    source: from,
                    target: dest.forge,
                    issue_on_target: &none,
                    origin_issue_url: &issue_url,
                };
                let body = render_comment_body(
                    &line,
                    &lcid,
                    dest.forge,
                    from,
                    comment.id,
                    &comment.body,
                    &rw,
                );
                pacing::pace_write(&mut *self.store, cfg, self.clock, dest.forge, &ns, false)?;
                let object = ObjectRef {
                    forge: dest.forge,
                    repo: dest.repo_id.clone(),
                    kind: Kind::Contribution,
                    number: dest.number,
                };
                self.ledger_write(dest.forge, &ns)?;
                let Some(adapter) = self.forges.get(dest.forge) else {
                    pending.push(format!("no adapter for {}", dest.forge.as_str()));
                    continue;
                };
                match adapter.create_comment(&object, &body) {
                    Ok(_) => {
                        created += 1;
                        written += 1;
                    }
                    Err(e) => {
                        self.after_error(dest.forge, &ns, &e)?;
                        pending.push(format!("{} comment: {e}", dest.forge.as_str()));
                    }
                }
            }
        }
        Ok(written)
    }
}
