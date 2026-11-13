//! The forge side of the relay: the narrow trait the drivers (cghb, cglb, cfgj; build task B3) implement.
//!
//! Like the existing bridge traits there is no generic mutation path: every write the router can make
//! is a method here, so a policy can enumerate them and a fake can record them. Reads and writes name
//! a repository by its immutable id (RF14); the adapter resolves it to the current path.

use super::heads::ReqState;
use super::merge::Patch;
use super::model::{Actor, CommentState, ObjectRef, ObjectState, Snap};
use crate::model::Forge;
use std::{collections::BTreeMap, fmt};

/// Why a forge said no.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// 401: the credential is not accepted.
    Unauthorized,
    /// 403, including a flagged account.
    Forbidden,
    /// 429 and secondary rate limits.
    RateLimited,
    /// 402: a plan limit.
    PaymentRequired,
    /// An abuse or spam detection answer.
    Abuse,
}

/// A failed call. Only [`ForgeError::Refused`] puts a namespace on hold (RD11, RF9); an outage is RF1
/// (the intent stays pending, no retry loop); a rejection is permanent for this payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgeError {
    Refused(Refusal, String),
    /// 5xx, a timeout, a broken connection. The write may or may not have happened.
    Unavailable(String),
    /// 4xx other than the above (validation): the same payload will not work.
    Rejected(String),
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(kind, detail) => write!(f, "refused ({kind:?}): {detail}"),
            Self::Unavailable(detail) => write!(f, "unavailable: {detail}"),
            Self::Rejected(detail) => write!(f, "rejected: {detail}"),
        }
    }
}

impl std::error::Error for ForgeError {}

/// The answer to a read. `Gone` is only for a definitive 404/410: the object does not exist. A 403, a
/// 5xx, a timeout or a rename is an `Err`, never `Gone` (R50 deletions, the maintainer 2026-11-13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    Found(Box<ObjectState>),
    Gone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewObject {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Created {
    pub number: u64,
    pub url: String,
    /// The object as the forge now shows it.
    pub snap: Snap,
}

/// A pull or merge request as the contribution flow reads it (RL4, RL5). The commits are fetched from
/// `fetch_url` (a clean https URL; credentials are the git host's business) at `reference`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRead {
    pub number: u64,
    pub url: String,
    pub author: Actor,
    pub title: String,
    pub body: String,
    pub state: ReqState,
    pub head_sha: String,
    /// The source branch name; a relay head (`cforge/<forge>-<n>`) marks a sibling the relay itself opened.
    pub head_branch: String,
    /// The base branch.
    pub base: String,
    pub fetch_url: String,
    /// `refs/pull/<n>/head` or `refs/merge-requests/<n>/head`.
    pub reference: String,
    pub comments: Vec<CommentState>,
    pub updated_at: u64,
}

/// What the router needs from one forge. All methods are synchronous and bounded by the adapter.
pub trait RouterForge {
    fn forge(&self) -> Forge;

    /// A pull or merge request with its comments (complete, in creation order); `None` only on a definitive
    /// 404/410. Adapters without contribution support keep the default.
    fn read_request(
        &mut self,
        _repo: &str,
        _number: u64,
    ) -> Result<Option<RequestRead>, ForgeError> {
        Err(ForgeError::Rejected(
            "this adapter does not read pull requests".into(),
        ))
    }

    /// Current state of one object, with its comments (complete, in creation order).
    fn read(&mut self, object: &ObjectRef) -> Result<ReadOutcome, ForgeError>;

    /// Creates an issue by the relay identity. Labels are created on demand with the same name.
    fn create_object(&mut self, repo: &str, new: &NewObject) -> Result<Created, ForgeError>;

    /// Applies a non-empty patch to one object.
    fn update_object(&mut self, object: &ObjectRef, patch: &Patch) -> Result<(), ForgeError>;

    /// Creates a comment by the relay identity; returns its id.
    fn create_comment(&mut self, object: &ObjectRef, body: &str) -> Result<u64, ForgeError>;

    fn update_comment(&mut self, object: &ObjectRef, id: u64, body: &str)
        -> Result<(), ForgeError>;

    /// A comment that is already gone counts as deleted.
    fn delete_comment(&mut self, object: &ObjectRef, id: u64) -> Result<(), ForgeError>;

    /// An object that is already gone counts as deleted.
    fn delete_object(&mut self, object: &ObjectRef) -> Result<(), ForgeError>;

    /// Lost-response recovery: the object in `repo` whose relay-authored body carries this exact
    /// marker line, if any.
    fn find_by_marker(&mut self, repo: &str, marker: &str) -> Result<Option<Created>, ForgeError>;

    /// Is `actor` a maintainer (may delete others' posts) of this repository? `Ok(None)` = the forge
    /// cannot tell; the router then holds a deletion for the reviewer instead of cascading it.
    fn is_maintainer(&mut self, repo: &str, actor: &Actor) -> Result<Option<bool>, ForgeError>;
}

/// The forges a run talks to, one adapter per forge. A forge without an entry is unreachable: the
/// router treats it as an outage (RF1), never as permission to skip.
#[derive(Default)]
pub struct Forges {
    map: BTreeMap<Forge, Box<dyn RouterForge>>,
}

impl Forges {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, adapter: Box<dyn RouterForge>) {
        self.map.insert(adapter.forge(), adapter);
    }

    pub fn get(&mut self, forge: Forge) -> Option<&mut (dyn RouterForge + 'static)> {
        self.map.get_mut(&forge).map(|b| &mut **b)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// R51: the reviewer's first pass (spam, malware, abuse; not a code review).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScreenVerdict {
    Pass,
    Hold(String),
}

pub trait Screen {
    fn screen(&self, forge: Forge, object: &ObjectState) -> ScreenVerdict;
}

/// Fail closed: nothing is fanned out until a screen has said so. The default until the reviewer exists (RL5).
pub struct HoldAll;

impl Screen for HoldAll {
    fn screen(&self, _forge: Forge, _object: &ObjectState) -> ScreenVerdict {
        ScreenVerdict::Hold("no abuse screen is attached".into())
    }
}

/// Pass-through screen (`screen: rate_limit`): the per-author cap of the engine is the only brake.
pub struct PassThrough;

impl Screen for PassThrough {
    fn screen(&self, _forge: Forge, _object: &ObjectState) -> ScreenVerdict {
        ScreenVerdict::Pass
    }
}

/// Maps an HTTP status of a failed call to the router's error classes. A 404 or 410 is NOT mapped to `Gone` here:
/// only the read and delete calls of a driver treat it as such, after looking at the object they asked for.
pub fn from_status(status: u16, detail: String) -> ForgeError {
    let lower = detail.to_ascii_lowercase();
    match status {
        401 => ForgeError::Refused(Refusal::Unauthorized, detail),
        402 => ForgeError::Refused(Refusal::PaymentRequired, detail),
        403 if lower.contains("abuse") || lower.contains("spam") => {
            ForgeError::Refused(Refusal::Abuse, detail)
        }
        403 if lower.contains("rate limit") => ForgeError::Refused(Refusal::RateLimited, detail),
        403 => ForgeError::Refused(Refusal::Forbidden, detail),
        429 => ForgeError::Refused(Refusal::RateLimited, detail),
        408 | 500..=599 => ForgeError::Unavailable(format!("{status}: {detail}")),
        _ => ForgeError::Rejected(format!("{status}: {detail}")),
    }
}

/// `2026-11-13T09:30:00Z` (also with fractions and numeric offsets) as seconds since the Unix epoch; 0 when it does
/// not parse. The forges' timestamps are all RFC 3339.
pub fn parse_rfc3339(text: &str) -> u64 {
    fn num(s: &str) -> Option<i64> {
        s.parse().ok()
    }
    let parse = || -> Option<i64> {
        let (date, rest) = text.split_once(['T', 't', ' '])?;
        let mut d = date.split('-');
        let (y, m, day) = (num(d.next()?)?, num(d.next()?)?, num(d.next()?)?);
        let split = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
        let (clock, zone) = rest.split_at(split);
        let mut c = clock.split(':');
        let (h, mi) = (num(c.next()?)?, num(c.next()?)?);
        let s = c
            .next()
            .map(|s| s.split('.').next().unwrap_or("0"))
            .and_then(num)
            .unwrap_or(0);
        let offset = match zone.chars().next() {
            Some(sign @ ('+' | '-')) => {
                let (oh, om) = zone[1..].split_once(':').unwrap_or((&zone[1..], "0"));
                let secs = num(oh)? * 3600 + num(om)? * 60;
                if sign == '+' {
                    secs
                } else {
                    -secs
                }
            }
            _ => 0,
        };
        // Days from civil (Howard Hinnant).
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        Some(days * 86_400 + h * 3600 + mi * 60 + s - offset)
    };
    parse().and_then(|v| u64::try_from(v).ok()).unwrap_or(0)
}

/// Is `marker` one whole line of `body` (the lost-response recovery of [`RouterForge::find_by_marker`])?
pub fn has_marker_line(body: &str, marker: &str) -> bool {
    body.lines().any(|l| l.trim() == marker)
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn timestamps_and_statuses() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), 0);
        assert_eq!(parse_rfc3339("2026-11-13T09:30:00Z"), 1_794_562_200);
        assert_eq!(
            parse_rfc3339("2026-11-13T10:30:00.123+01:00"),
            1_794_562_200
        );
        assert_eq!(parse_rfc3339("garbage"), 0);
        assert!(matches!(
            from_status(403, "You have exceeded a secondary rate limit".into()),
            ForgeError::Refused(Refusal::RateLimited, _)
        ));
        assert!(matches!(
            from_status(404, "x".into()),
            ForgeError::Rejected(_)
        ));
        assert!(matches!(
            from_status(502, "x".into()),
            ForgeError::Unavailable(_)
        ));
    }
}
