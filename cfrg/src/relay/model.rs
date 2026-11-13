//! Router object model: identifiers, hints, the configuration and the rows of the state store
//! (Forge-router §4 and §6). Nothing here touches the network, the clock or the filesystem.

use crate::model::Forge;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

/// Logical object or comment id: a UUID derived from the origin, so a retried creation finds its own marker.
pub type Lid = String;

/// What a logical object is. `Contribution` is part of the model; its flows are lanes RL4 and RL5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Issue,
    Contribution,
}

/// One object on one forge. `repo` is the forge's immutable repository id (RF14), never a path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectRef {
    pub forge: Forge,
    pub repo: String,
    pub kind: Kind,
    pub number: u64,
}

/// A person or machine account on one forge: the immutable id decides, the login is for humans.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Actor {
    pub id: String,
    pub login: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    Open,
    Closed,
}

/// What a delivery says happened. A delivery is only a hint (RD1); `Deleted*` additionally names the
/// deleting actor where the forge exposes it, but is never evidence by itself (R50, see the cascade).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HintAction {
    Changed,
    Deleted,
    CommentDeleted {
        comment_id: u64,
    },
    /// Internal: catch-up, sweep and the owner's release of a hold. Carries no actor.
    Reconcile,
}

/// The hint (RD1): forge, repository id, object kind and number. Everything else is read from the forge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hint {
    pub forge: Forge,
    pub repo: String,
    pub kind: Kind,
    pub number: u64,
    pub action: HintAction,
    #[serde(default)]
    pub actor: Option<Actor>,
}

impl Hint {
    pub fn object(&self) -> ObjectRef {
        ObjectRef {
            forge: self.forge,
            repo: self.repo.clone(),
            kind: self.kind,
            number: self.number,
        }
    }
}

// --- configuration ---------------------------------------------------------------------------

/// One repository of one forge, by immutable id (RF14) and current path (`namespace/name`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoRef {
    /// Web host of the forge instance, e.g. `github.com`; the R11 contributed-project check needs it.
    pub host: String,
    pub id: String,
    pub path: String,
}

impl RepoRef {
    /// The pacing and hold namespace: organisation, top-level group or workspace.
    pub fn namespace(&self) -> &str {
        self.path.split('/').next().unwrap_or("")
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Credit {
    /// "**@user** wrote this on GitLab: <link>" (R41 default).
    #[default]
    Highlight,
    /// "Relayed from GitLab: <link>": the project opted out of contributor highlighting.
    Plain,
}

/// One replicated repository: its primary on Forgejo (R44) and the copies. A repository that is not
/// listed is never relayed (R2 exclusions, R11 contributed projects, RD12).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoSet {
    /// Name of the primary, `org/repo` on Forgejo.
    pub name: String,
    #[serde(default)]
    pub credit: Credit,
    /// Must contain `forgejo` (the hub).
    pub members: BTreeMap<Forge, RepoRef>,
}

/// One row of the identity table (`cfrg access`): a person and their handle on each forge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Person {
    pub key: String,
    pub name: String,
    pub handles: BTreeMap<Forge, String>,
}

/// Mutation caps per namespace (RD11, RF9): far below GitHub's documented 80/min and 500/h. A forge's first refusal
/// holds the namespace; the caps count the ledger of this store, so every relay job shares them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Pacing {
    pub namespace_per_hour: u64,
    pub namespace_per_day: u64,
    /// Minimum seconds between any two writes to one forge namespace, across all relay jobs (kept in the store).
    pub write_gap_secs: u64,
    /// Minimum seconds between two object creations in one forge namespace.
    pub create_gap_secs: u64,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            namespace_per_hour: 60,
            namespace_per_day: 400,
            write_gap_secs: 2,
            create_gap_secs: 12,
        }
    }
}

/// Counted limits that are not pacing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// R51: new objects of one author fanned out per day; the rest is held on Forgejo.
    pub author_fanout_per_day: u64,
    /// RF8 backstop, tuned in RL2: write passes on one logical object within an hour before the object
    /// is frozen. The page proposed "20 writes"; one honest pass writes once per sibling and per new
    /// comment, so counting passes (a loop is one per cycle) keeps a busy thread from tripping it.
    pub object_write_passes_per_hour: u64,
    /// R50 cascade: logical deletions that are carried to the other copies, fleet-wide per hour.
    pub cascade_deletes_per_hour: u64,
    /// R50 cascade: the same, per primary repository.
    pub cascade_deletes_per_repo_per_hour: u64,
    /// Seconds a writer holds the lease of one logical object (a crashed job frees it by expiry).
    pub lease_secs: u64,
    /// Sweep: how old a pending intent must be, and how many objects one sweep drives.
    pub sweep_older_than_secs: u64,
    pub sweep_max_objects: usize,
    /// Comments read per object are bounded by the adapter; this bounds what one pass relays.
    pub max_new_comments_per_pass: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            author_fanout_per_day: 5,
            object_write_passes_per_hour: 30,
            cascade_deletes_per_hour: 10,
            cascade_deletes_per_repo_per_hour: 3,
            lease_secs: 600,
            sweep_older_than_secs: 3_600,
            sweep_max_objects: 50,
            max_new_comments_per_pass: 100,
        }
    }
}

/// The router configuration (a JSON file). Credentials are never in it (R31).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterConfig {
    /// Immutable user ids of the relay identity (`relay-bot`) per forge. Guard RD4(a) and the
    /// RD9 marker trust compare ids, never logins.
    pub relay: BTreeMap<Forge, Vec<String>>,
    /// Left out in production: the sets come from the placement ([`super::derive::repos_from_placement`]).
    #[serde(default)]
    pub repos: Vec<RepoSet>,
    #[serde(default)]
    pub people: Vec<Person>,
    #[serde(default)]
    pub pacing: Pacing,
    #[serde(default)]
    pub limits: Limits,
    /// This path exists = the router makes no request at all (reads included). Operators `mkdir` it.
    #[serde(default)]
    pub kill_switch: Option<PathBuf>,
    /// Label that marks an object held on Forgejo (R51). Never relayed.
    #[serde(default = "default_held_label")]
    pub held_label: String,
    /// Labels that exist on one forge only and are never merged across forges.
    #[serde(default = "default_local_labels")]
    pub local_labels: Vec<String>,
    /// R51 abuse screen: `hold_all` (default, fail closed) or `rate_limit` (pass-through; the per-author cap of
    /// `limits.author_fanout_per_day` is the only brake, the rest is held on Forgejo).
    #[serde(default)]
    pub screen: ScreenKind,
}

/// Which abuse screen the relay runs (R51).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenKind {
    #[default]
    HoldAll,
    RateLimit,
}

fn default_held_label() -> String {
    "held".into()
}

fn default_local_labels() -> Vec<String> {
    vec!["held".into(), "contribution".into()]
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            relay: BTreeMap::new(),
            repos: Vec::new(),
            people: Vec::new(),
            pacing: Pacing::default(),
            limits: Limits::default(),
            kill_switch: None,
            held_label: default_held_label(),
            local_labels: default_local_labels(),
            screen: ScreenKind::default(),
        }
    }
}

impl RouterConfig {
    pub fn parse(text: &str) -> crate::Result<Self> {
        let config: Self = serde_json::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> crate::Result<()> {
        let mut seen: BTreeSet<(Forge, &str)> = BTreeSet::new();
        for set in &self.repos {
            crate::gate::split(&set.name)?;
            if !set.members.contains_key(&Forge::Forgejo) {
                return Err(crate::failure(format!(
                    "{}: a replicated repository needs its Forgejo primary (R44)",
                    set.name
                )));
            }
            for (forge, repo) in &set.members {
                if repo.host.is_empty()
                    || repo.id.is_empty()
                    || repo.path.is_empty()
                    || repo.namespace().is_empty()
                {
                    return Err(crate::failure(format!(
                        "{}: the {} member needs a host, an id and a path",
                        set.name,
                        forge.as_str()
                    )));
                }
                if !seen.insert((*forge, repo.id.as_str())) {
                    return Err(crate::failure(format!(
                        "{}: {} repository id {} appears twice",
                        set.name,
                        forge.as_str(),
                        repo.id
                    )));
                }
            }
        }
        for forge in [Forge::Forgejo, Forge::Github, Forge::Gitlab] {
            if self.relay.get(&forge).is_none_or(Vec::is_empty)
                && self.repos.iter().any(|s| s.members.contains_key(&forge))
            {
                return Err(crate::failure(format!(
                    "No relay identity is configured for {}",
                    forge.as_str()
                )));
            }
        }
        Ok(())
    }

    /// The policy check, before any credential or request: is this repository one the router serves?
    pub fn locate(&self, forge: Forge, repo_id: &str) -> Option<&RepoSet> {
        self.repos
            .iter()
            .find(|s| s.members.get(&forge).is_some_and(|m| m.id == repo_id))
    }

    pub fn set_named(&self, name: &str) -> Option<&RepoSet> {
        self.repos.iter().find(|s| s.name == name)
    }

    pub fn is_relay(&self, forge: Forge, actor: &Actor) -> bool {
        self.relay
            .get(&forge)
            .is_some_and(|ids| ids.contains(&actor.id))
    }

    pub fn person_by_handle(&self, forge: Forge, login: &str) -> Option<&Person> {
        self.people.iter().find(|p| {
            p.handles
                .get(&forge)
                .is_some_and(|h| h.eq_ignore_ascii_case(login))
        })
    }

    pub fn person(&self, key: &str) -> Option<&Person> {
        self.people.iter().find(|p| p.key == key)
    }

    pub fn is_local_label(&self, name: &str) -> bool {
        self.held_label == name || self.local_labels.iter().any(|l| l == name)
    }
}

/// Forges that carry issues. Bitbucket Cloud removed Issues and Wikis on 2026-08-20 (model §3.5).
pub fn has_issues(forge: Forge) -> bool {
    !matches!(forge, Forge::Bitbucket)
}

pub fn forge_title(forge: Forge) -> &'static str {
    match forge {
        Forge::Github => "GitHub",
        Forge::Forgejo => "Forgejo",
        Forge::Gitlab => "GitLab",
        Forge::Bitbucket => "Bitbucket",
    }
}

// --- state as read from a forge ---------------------------------------------------------------

/// The text and flags of one object as the router tracks them. Bodies are raw, as the forge shows them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snap {
    pub title: String,
    pub body: String,
    pub state: Option<IssueState>,
    pub labels: BTreeSet<String>,
    /// Handles on this forge.
    pub assignees: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentState {
    pub id: u64,
    pub url: String,
    pub body: String,
    pub author: Actor,
    pub created_at: u64,
}

/// One object as read: the answer to a hint (RD1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectState {
    pub number: u64,
    pub url: String,
    pub author: Actor,
    pub snap: Snap,
    /// Confidential issues are never relayed (RD12, R50).
    pub confidential: bool,
    pub comments: Vec<CommentState>,
    pub updated_at: u64,
}

// --- store rows -------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjState {
    Open,
    Closed,
    TakenIn,
}

/// How far a hold on one object reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldScope {
    /// R51: the object stays on Forgejo only; changes still reach the hub copy.
    FanOut,
    /// RF8 and held deletions: no write at all until the owner releases the object.
    Frozen,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectHold {
    pub reason: String,
    pub scope: HoldScope,
    pub since: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub at: u64,
    pub reason: String,
}

/// The merged truth of an object (RD3), in origin terms: what every sibling is converged to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Canon {
    pub title: String,
    pub body: String,
    pub state: Option<IssueState>,
    pub labels: BTreeSet<String>,
    /// Identity-table person keys. Handles the table does not know are not relayed.
    pub assignees: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalObject {
    pub lid: Lid,
    pub kind: Kind,
    /// Name of the `RepoSet`.
    pub primary: String,
    pub origin: ObjectRef,
    pub origin_author: Actor,
    pub state: ObjState,
    pub screened: bool,
    pub held: Option<ObjectHold>,
    pub tombstone: Option<Tombstone>,
    pub canon: Canon,
    pub created: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiblingRow {
    pub lid: Lid,
    pub forge: Forge,
    pub repo: String,
    pub number: u64,
    /// The human-authored object (the origin); every other sibling is a copy by the relay identity.
    pub origin: bool,
    pub url: String,
    /// The origin's cforge comment that lists the siblings (copies carry the list in their body).
    #[serde(default)]
    pub list_comment: Option<u64>,
    /// The sibling as the router last knew it, including the router's own writes. A local change is
    /// the current state minus this snapshot (RD3).
    pub snapshot: Option<Snap>,
    /// Why this sibling is behind the canon, if it is.
    #[serde(default)]
    pub behind: Option<String>,
    #[serde(default)]
    pub deleted: bool,
    pub last_synced: u64,
}

impl SiblingRow {
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.forge.as_str(), self.repo, self.number)
    }

    pub fn object_ref(&self, kind: Kind) -> ObjectRef {
        ObjectRef {
            forge: self.forge,
            repo: self.repo.clone(),
            kind,
            number: self.number,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentCopy {
    pub id: u64,
    /// The copy's body as the router last wrote or saw it.
    pub body: String,
}

/// A comment relayed as an append-only set keyed by origin (RD3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentRow {
    pub lcid: Lid,
    pub lid: Lid,
    pub origin_forge: Forge,
    pub origin_id: u64,
    pub origin_url: String,
    pub author: Actor,
    pub origin_body: String,
    pub created_at: u64,
    pub copies: BTreeMap<Forge, CommentCopy>,
    #[serde(default)]
    pub tombstone: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    CreateObject,
    UpdateObject,
    CreateComment,
    UpdateComment,
    CreateList,
    UpdateList,
    DeleteComment,
    DeleteObject,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub forge: Forge,
    pub repo: String,
    pub number: u64,
    /// Comment id, or the origin comment's `lcid` for a creation that has no id yet.
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentStatus {
    Planned,
    Done,
    /// The forge refused this payload for good (a validation error).
    Failed,
    Superseded,
}

/// One intended write. Committed before the request (§6); resolved by the answer or, after a lost
/// answer, by reading the target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    /// A UUID made by the writer (never a sequence: pgEdge is multi-master).
    pub id: String,
    pub lid: Lid,
    pub target: Target,
    pub op: OpKind,
    pub digest: String,
    pub created: u64,
    pub attempts: u32,
    pub status: IntentStatus,
    pub resolved: Option<u64>,
    pub note: Option<String>,
}

impl Intent {
    pub fn same_slot(&self, target: &Target, op: OpKind) -> bool {
        self.op == op && self.target == *target
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// A mutation request that left the router (counted before it is sent, so a crash over-counts).
    Write,
    /// One pass of an object that wrote at least once (RF8).
    WritePass,
    /// A new object fanned out for an author (R51).
    FanOut,
    /// A logical deletion carried to the other copies (R50).
    Cascade,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub ts: u64,
    pub kind: LedgerKind,
    pub forge: Forge,
    pub namespace: String,
    pub lid: Option<Lid>,
    /// Primary repository name for `Cascade`, author key `forge:id` for `FanOut`.
    pub detail: String,
}

/// A hold on one (forge, namespace) (RD11). The relay identity is one per forge, so the pair is the key.
/// Only the owner lifts it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub forge: Forge,
    pub namespace: String,
    pub reason: String,
    pub since: u64,
}

pub fn digest(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.len().to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A UUID derived from `parts` (SHA-256, version 8 layout), so a retried creation or a lost row finds the same key.
fn derived_uuid(parts: &[&str]) -> Lid {
    let hex = digest(parts);
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

pub fn lid_object(origin: &ObjectRef) -> Lid {
    derived_uuid(&[
        origin.forge.as_str(),
        &origin.repo,
        &origin.number.to_string(),
    ])
}

pub fn lid_comment(lid: &str, forge: Forge, comment_id: u64) -> Lid {
    derived_uuid(&[lid, forge.as_str(), &comment_id.to_string()])
}
