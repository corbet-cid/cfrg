//! The forge side of the relay: the narrow trait the drivers (cghb, cglb, cfgj; build task B3) implement.
//!
//! Like the existing bridge traits there is no generic mutation path: every write the router can make
//! is a method here, so a policy can enumerate them and a fake can record them. Reads and writes name
//! a repository by its immutable id (RF14); the adapter resolves it to the current path.

use super::merge::Patch;
use super::model::{Actor, ObjectRef, ObjectState, Snap};
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
/// 5xx, a timeout or a rename is an `Err`, never `Gone` (R50 deletions, richc-bc 2026-11-13).
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

/// What the router needs from one forge. All methods are synchronous and bounded by the adapter.
pub trait RouterForge {
    fn forge(&self) -> Forge;

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
