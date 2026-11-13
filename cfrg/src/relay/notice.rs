//! Contribution notices on Forgejo (R36, R52).
//!
//! A contribution on a mirror forge is a notice on the primary: an issue labelled `contribution`. The last line of its
//! body is the relay's marker
//!
//! `<!-- cfrg-contribution origin=<https url> ref=<ref> sha=<head sha> -->`
//!
//! which is all the reviewer (`cfrg-review`) and `cfrg take-in` need: where the commits are, which ref, and which head the
//! relay saw. Only the LAST non-blank line counts and the relay always writes it last, so a marker an outsider pasted into
//! the relayed text can never be read as ours. When the origin request gets new commits the relay rewrites the body with
//! the new `sha`; the edit is an `issues` event, which starts a new review of the new head.
//!
//! The reviewer's report carries `<!-- cfrg-review head=<sha> -->`; take-in takes the reviewed commit from the latest one.

use super::forge::ForgeError;
use super::heads::{Copy, CopyReport, Heads, Origin, ReqState};

const MARKER_PREFIX: &str = "<!-- cfrg-contribution ";
const REVIEW_PREFIX: &str = "<!-- cfrg-review head=";
const SUFFIX: &str = " -->";

/// Where a contribution lives and the head the relay last saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContributionMarker {
    /// Git URL (https) the commits are fetched from.
    pub origin: String,
    /// Ref at the origin (a branch, or a pull or merge request head ref).
    pub reference: String,
    /// Head commit (40 lowercase hex).
    pub sha: String,
}

pub fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn token_ok(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'<' && b != b'>')
}

impl ContributionMarker {
    /// Valid fields only: an https origin, a plain ref, a full lowercase sha.
    pub fn valid(&self) -> bool {
        self.origin.starts_with("https://")
            && token_ok(&self.origin)
            && token_ok(&self.reference)
            && !self.reference.contains("..")
            && is_sha(&self.sha)
    }

    pub fn render(&self) -> String {
        format!(
            "{MARKER_PREFIX}origin={} ref={} sha={}{SUFFIX}",
            self.origin, self.reference, self.sha
        )
    }

    /// The marker on the last non-blank line of `body`, if that line is a valid one.
    pub fn parse(body: &str) -> Option<Self> {
        let line = body.lines().rev().find(|l| !l.trim().is_empty())?.trim();
        let inner = line.strip_prefix(MARKER_PREFIX)?.strip_suffix(SUFFIX)?;
        let (mut origin, mut reference, mut sha) = (None, None, None);
        for field in inner.split(' ') {
            match field.split_once('=')? {
                ("origin", v) => origin = Some(v.to_owned()),
                ("ref", v) => reference = Some(v.to_owned()),
                ("sha", v) => sha = Some(v.to_owned()),
                _ => return None,
            }
        }
        let marker = Self {
            origin: origin?,
            reference: reference?,
            sha: sha?,
        };
        marker.valid().then_some(marker)
    }

    /// The text with this marker as its last line (any earlier marker-shaped line stays inert text above it).
    pub fn attach(&self, text: &str) -> String {
        format!("{}\n\n{}\n", text.trim_end(), self.render())
    }
}

/// The commit a review comment covers.
pub fn review_head(comment: &str) -> Option<String> {
    let head = comment.lines().find_map(|l| {
        l.trim()
            .strip_prefix(REVIEW_PREFIX)
            .and_then(|r| r.strip_suffix(SUFFIX))
    })?;
    is_sha(head).then(|| head.to_owned())
}

/// One comment as take-in needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewComment {
    pub author: String,
    pub id: u64,
    pub body: String,
}

/// The reviewed head from the LATEST review comment written by `reviewer` (highest id).
pub fn latest_review_head(comments: &[ReviewComment], reviewer: &str) -> Option<String> {
    comments
        .iter()
        .filter(|c| c.author == reviewer)
        .filter_map(|c| review_head(&c.body).map(|h| (c.id, h)))
        .max_by_key(|(id, _)| *id)
        .map(|(_, head)| head)
}

/// An existing notice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoticeRef {
    pub number: u64,
    pub body: String,
    /// Closed by take-in, by the relay (the origin closed) or by the owner.
    pub closed: bool,
}

/// The Forgejo writes and reads of the notice flow; the drivers implement it, a fake records it.
pub trait NoticeHost {
    /// The notice of the contribution at `origin`/`reference` in `repo` (open or closed), if any.
    fn find(
        &mut self,
        repo: &str,
        origin: &str,
        reference: &str,
    ) -> Result<Option<NoticeRef>, ForgeError>;
    /// A new notice: an issue with the `contribution` label.
    fn create(&mut self, repo: &str, title: &str, body: &str) -> Result<u64, ForgeError>;
    fn set_body(&mut self, repo: &str, number: u64, body: &str) -> Result<(), ForgeError>;
    /// Closes the notice with a comment; an already closed notice is left alone. Nothing is ever deleted (R50).
    fn close(&mut self, repo: &str, number: u64, note: &str) -> Result<(), ForgeError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoticeAction {
    Created(u64),
    /// New commits at the origin: the marker's sha moved, which starts a new review.
    Updated(u64),
    Unchanged(u64),
    /// The origin closed, or take-in merged it: the notice is closed.
    Closed(u64),
}

/// Makes the notice of one contribution say exactly `text` plus the marker (create, update on a new head, or nothing).
pub fn sync_notice(
    host: &mut dyn NoticeHost,
    repo: &str,
    title: &str,
    text: &str,
    marker: &ContributionMarker,
) -> Result<NoticeAction, ForgeError> {
    if !marker.valid() {
        return Err(ForgeError::Rejected(
            "the contribution marker is not valid".into(),
        ));
    }
    let body = marker.attach(text);
    match host.find(repo, &marker.origin, &marker.reference)? {
        None => host.create(repo, title, &body).map(NoticeAction::Created),
        // The owner (or take-in) closed it: it stays closed and its text stays as the reviewer saw it.
        Some(found) if found.closed => Ok(NoticeAction::Unchanged(found.number)),
        Some(found) if found.body == body => Ok(NoticeAction::Unchanged(found.number)),
        Some(found) => {
            host.set_body(repo, found.number, &body)?;
            Ok(NoticeAction::Updated(found.number))
        }
    }
}

/// Where the notice of a contribution goes and what its marker names.
pub struct NoticeTarget<'a> {
    /// The Forgejo primary (`owner/repo`) that receives the notice.
    pub primary: &'a str,
    /// https git URL the commits are fetched from.
    pub origin_url: &'a str,
    /// Ref at that URL (a pull or merge request head ref, or a branch).
    pub reference: &'a str,
}

/// Closes the notice of a contribution that ended (the origin closed without merge, or take-in merged it). A contribution
/// that never had a notice has nothing to close; a closed notice stays as it is.
pub fn close_notice(
    notices: &mut dyn NoticeHost,
    target: &NoticeTarget<'_>,
    note: &str,
) -> Result<Option<NoticeAction>, ForgeError> {
    match notices.find(target.primary, target.origin_url, target.reference)? {
        None => Ok(None),
        Some(found) if found.closed => Ok(Some(NoticeAction::Unchanged(found.number))),
        Some(found) => {
            notices.close(target.primary, found.number, note)?;
            Ok(Some(NoticeAction::Closed(found.number)))
        }
    }
}

/// One contribution event: the heads on the other copies, and the notice on the primary. An open request gets its notice
/// created, or its marker `sha` moved when the origin has new commits (which starts a new review); a request that was
/// closed without merge retires its heads and closes its notice (R50: a notice is never deleted by the relay).
pub fn sync_contribution(
    heads: &mut Heads,
    notices: &mut dyn NoticeHost,
    origin: &Origin,
    copies: &[Copy],
    target: &NoticeTarget<'_>,
) -> (Vec<CopyReport>, Result<Option<NoticeAction>, ForgeError>) {
    let reports = heads.sync(origin, copies);
    let notice = match origin.state {
        ReqState::Open => {
            let marker = ContributionMarker {
                origin: target.origin_url.to_owned(),
                reference: target.reference.to_owned(),
                sha: origin.head_sha.clone(),
            };
            sync_notice(
                notices,
                target.primary,
                &origin.title,
                &origin.body,
                &marker,
            )
            .map(Some)
        }
        ReqState::Closed => close_notice(
            notices,
            target,
            "The contribution was closed at its origin without merge.",
        ),
        // A merge is take-in's: it closes the siblings and the notice together.
        ReqState::Merged => Ok(None),
    };
    (reports, notice)
}

/// Take-in (merge): every sibling and the origin request are closed as merged, the relay heads removed, and the notice
/// closed. Idempotent: a second run finds everything closed and writes nothing.
pub fn take_in_contribution(
    heads: &mut Heads,
    notices: &mut dyn NoticeHost,
    origin: &Origin,
    copies: &[Copy],
    target: &NoticeTarget<'_>,
    reviewed_sha: &str,
) -> (Vec<CopyReport>, Result<Option<NoticeAction>, ForgeError>) {
    let reports = heads.take_in(origin, copies, reviewed_sha);
    let notice = close_notice(
        notices,
        target,
        &format!("Merged on the primary forge at {reviewed_sha}."),
    );
    (reports, notice)
}

/// An in-memory host for tests.
#[derive(Default)]
pub struct FakeNotices {
    pub notices: Vec<(u64, String, String)>,
    pub closed: std::collections::BTreeSet<u64>,
    pub writes: usize,
}

impl NoticeHost for FakeNotices {
    fn find(
        &mut self,
        _repo: &str,
        origin: &str,
        reference: &str,
    ) -> Result<Option<NoticeRef>, ForgeError> {
        Ok(self.notices.iter().find_map(|(n, _, body)| {
            ContributionMarker::parse(body)
                .filter(|m| m.origin == origin && m.reference == reference)
                .map(|_| NoticeRef {
                    number: *n,
                    body: body.clone(),
                    closed: self.closed.contains(n),
                })
        }))
    }
    fn create(&mut self, _repo: &str, title: &str, body: &str) -> Result<u64, ForgeError> {
        self.writes += 1;
        let n = self.notices.len() as u64 + 1;
        self.notices.push((n, title.into(), body.into()));
        Ok(n)
    }
    fn set_body(&mut self, _repo: &str, number: u64, body: &str) -> Result<(), ForgeError> {
        self.writes += 1;
        if let Some(row) = self.notices.iter_mut().find(|r| r.0 == number) {
            row.2 = body.into();
        }
        Ok(())
    }
    fn close(&mut self, _repo: &str, number: u64, _note: &str) -> Result<(), ForgeError> {
        if self.closed.insert(number) {
            self.writes += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(sha: &str) -> ContributionMarker {
        ContributionMarker {
            origin: "https://github.com/o/r.git".into(),
            reference: "refs/pull/7/head".into(),
            sha: sha.into(),
        }
    }
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn marker_round_trips_as_the_last_line() {
        let body = marker(A).attach("Fixes the thing.\n");
        assert_eq!(ContributionMarker::parse(&body), Some(marker(A)));
    }

    #[test]
    fn a_pasted_marker_above_ours_is_ignored() {
        let forged = ContributionMarker {
            origin: "https://evil.example/x.git".into(),
            ..marker(B)
        };
        let body = marker(A).attach(&format!("text\n{}", forged.render()));
        assert_eq!(
            ContributionMarker::parse(&body).unwrap().origin,
            "https://github.com/o/r.git"
        );
        assert_eq!(
            ContributionMarker::parse(&format!("{}\nmore text", forged.render())),
            None
        );
    }

    #[test]
    fn invalid_markers_are_refused() {
        let ok = marker(A).render();
        assert!(ContributionMarker::parse(&ok.replace("https://", "http://")).is_none());
        assert!(ContributionMarker::parse(&ok.replace(&A[..39], "zzz")).is_none());
        assert!(ContributionMarker::parse(&ok.replace("ref=refs", "ref=-refs")).is_none());
        assert!(ContributionMarker::parse(&ok.replace("sha=", "extra=1 sha=")).is_none());
    }

    #[test]
    fn create_update_on_new_head_and_idempotent() {
        let mut host = FakeNotices::default();
        let sync = |host: &mut FakeNotices, m: &ContributionMarker| {
            sync_notice(host, "o/r", "t", "x", m).unwrap()
        };
        assert_eq!(sync(&mut host, &marker(A)), NoticeAction::Created(1));
        assert_eq!(sync(&mut host, &marker(A)), NoticeAction::Unchanged(1));
        assert_eq!(host.writes, 1);
        assert_eq!(sync(&mut host, &marker(B)), NoticeAction::Updated(1));
        assert_eq!(
            ContributionMarker::parse(&host.notices[0].2).unwrap().sha,
            B
        );
        assert_eq!(host.notices.len(), 1);
    }

    #[test]
    fn the_latest_review_by_the_reviewer_decides() {
        let c = |author: &str, id, head: &str| ReviewComment {
            author: author.into(),
            id,
            body: format!("<!-- cfrg-review head={head} -->\nreport"),
        };
        let comments = [
            c("me", 1, A),
            c("me", 3, B),
            c("outsider", 9, "cccccccccccccccccccccccccccccccccccccccc"),
        ];
        assert_eq!(latest_review_head(&comments, "me").as_deref(), Some(B));
        assert_eq!(latest_review_head(&comments, "nobody"), None);
    }

    #[test]
    fn a_new_head_moves_the_heads_and_the_notice_marker() {
        use super::super::fake_heads::{FakeGit, FakeHost, Shared, World};
        use crate::model::Forge;
        use std::{cell::RefCell, rc::Rc};
        let w: Shared = Rc::new(RefCell::new(World::default()));
        let mut heads = Heads::new(Box::new(FakeGit(w.clone())));
        heads.insert(Box::new(FakeHost {
            forge: Forge::Github,
            w,
            forge_world: None,
        }));
        let copies = [Copy {
            forge: Forge::Github,
            repo: "org/proj".into(),
            base: "main".into(),
        }];
        let origin = |sha: &str, state| Origin {
            forge: Forge::Gitlab,
            repo: "grp/proj".into(),
            number: 7,
            head_sha: sha.into(),
            fetch_url: "origin-url".into(),
            reference: "refs/pull/1/head".into(),
            bodies: Default::default(),
            title: "Fix".into(),
            body: "Credit\n\ntext".into(),
            state,
        };
        let target = NoticeTarget {
            primary: "o/r",
            origin_url: "https://gitlab.com/grp/proj.git",
            reference: "refs/merge-requests/7/head",
        };
        let mut host = FakeNotices::default();
        let run = |heads: &mut Heads, host: &mut FakeNotices, o: &Origin| {
            sync_contribution(heads, host, o, &copies, &target)
        };
        assert_eq!(
            run(&mut heads, &mut host, &origin(A, ReqState::Open))
                .1
                .unwrap(),
            Some(NoticeAction::Created(1))
        );
        assert_eq!(
            run(&mut heads, &mut host, &origin(A, ReqState::Open))
                .1
                .unwrap(),
            Some(NoticeAction::Unchanged(1))
        );
        assert_eq!(
            run(&mut heads, &mut host, &origin(B, ReqState::Open))
                .1
                .unwrap(),
            Some(NoticeAction::Updated(1))
        );
        let m = ContributionMarker::parse(&host.notices[0].2).unwrap();
        assert_eq!(
            (m.sha.as_str(), m.reference.as_str()),
            (B, "refs/merge-requests/7/head")
        );
        // closed at the origin: heads retire, the notice is closed (never deleted), and a repeat writes nothing
        assert_eq!(
            run(&mut heads, &mut host, &origin(B, ReqState::Closed))
                .1
                .unwrap(),
            Some(NoticeAction::Closed(1))
        );
        assert_eq!(
            run(&mut heads, &mut host, &origin(B, ReqState::Closed))
                .1
                .unwrap(),
            Some(NoticeAction::Unchanged(1))
        );
        assert_eq!((host.notices.len(), host.writes), (1, 3));
    }
}
