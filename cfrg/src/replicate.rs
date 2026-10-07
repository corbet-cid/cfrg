//! Replication across forges: the forge-neutral vocabulary and the per-edge
//! choice of mechanism.
//!
//! Every forge adapter declares what it can do natively ([`Capabilities`]).
//! For each edge of a placement (one sender, one receiver) cfrg picks the most
//! native mechanism, in this order:
//!
//! 1. the sender's own **push mirror** ([`Method::SourcePushMirror`]);
//! 2. the receiver's own **pull mirror** ([`Method::DestinationPullMirror`]),
//!    only where the forge can turn the repository in question into one;
//! 3. **`cfrg sync`** ([`Method::Sync`]), the adapter-only fast-forward copy.
//!
//! The same vocabulary drives [`crate::switch`]. Nothing here touches the
//! network, the clock or the filesystem.
use crate::{
    failure,
    land::{Capability, Support},
    model::Forge,
    native::http::Transport,
    Result,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// What one forge adapter provides for replication. Declared once per adapter,
/// printed by `cfrg switch --capabilities`.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Capabilities {
    pub forge: Forge,
    /// The forge pushes its own repository to another repository by itself.
    pub push_mirror: Capability,
    /// The sender can mint a mirror key that the receiver then admits
    /// exclusively (a deploy key). Without it the mirror authenticates with a
    /// stored credential of the receiver's mirror principal.
    pub mirror_key: bool,
    /// The forge keeps a repository current by fetching from another one.
    pub pull_mirror: Capability,
    /// A pull mirror can be set up on an EXISTING repository.
    pub pull_mirror_converts_existing: bool,
    /// A receiving repository can be locked so that only the mirror principal
    /// may write to it (deploy key, user or bot, depending on the forge).
    pub receiver_lock: Capability,
    /// Becoming primary again, and turning the old primary into a receiver.
    pub switch: Capability,
    /// Renaming this forge's copy of a repository in place, so that it keeps
    /// the name of its primary (`cfrg native`, `docs/native-mirrors.md`).
    pub rename: Capability,
}

fn native(support: Support) -> bool {
    matches!(support, Support::Native | Support::NativeFill)
}

/// How one edge replicates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    /// The sender's push mirror (source-native).
    SourcePushMirror,
    /// The receiver's pull mirror (destination-native).
    DestinationPullMirror,
    /// `cfrg sync`: fast-forward-only Git copy driven from outside the forges.
    Sync,
}

/// Pick the mechanism for one edge: source push mirror, then destination pull
/// mirror, then `cfrg sync`. `receiver_exists` is false only while a
/// replica is being created: an existing repository cannot become a pull
/// mirror on a forge that offers them at creation only.
pub fn choose(sender: &Capabilities, receiver: &Capabilities, receiver_exists: bool) -> Method {
    if native(sender.push_mirror.support) {
        Method::SourcePushMirror
    } else if native(receiver.pull_mirror.support)
        && (!receiver_exists || receiver.pull_mirror_converts_existing)
    {
        Method::DestinationPullMirror
    } else {
        Method::Sync
    }
}

/// One repository on one forge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    pub forge: Forge,
    /// API origin, `https://host`.
    pub origin: String,
    /// Public web base as `primaries` in the placement names it.
    pub web: String,
    /// Owner and repository as the forge spells them.
    pub path: String,
    /// Immutable identity once known (Forgejo and GitLab project id, Bitbucket uuid).
    pub id: Option<String>,
    /// Git host and path without scheme, credentials or `.git`: what a mirror
    /// address is compared with.
    pub address: String,
}

impl Site {
    pub fn new(
        forge: Forge,
        origin: &str,
        web: &str,
        git_host: &str,
        path: &str,
        id: Option<String>,
    ) -> Self {
        Self {
            forge,
            origin: origin.trim_end_matches('/').into(),
            web: web.trim_end_matches('/').into(),
            path: path.into(),
            id,
            address: format!("{}/{}", git_host.to_ascii_lowercase(), path),
        }
    }

    /// Key under which the mirror from `self` to `to` is recorded as owned.
    /// For a Forgejo sender this is the key `cfrg native` has always used.
    pub fn mirror_key(&self, to: &Site) -> String {
        format!(
            "{}/{}/{}/{}",
            self.origin,
            self.id.as_deref().unwrap_or(&self.path),
            to.origin,
            to.path
        )
    }
}

/// `host/path` of a Git URL or a mirror address: scheme, user information
/// (including the masked `*****:*****@` GitLab prints) and `.git` removed.
pub fn address_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let (authority, path) = rest.split_once('/')?;
    let host = authority.rsplit('@').next()?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{}/{}", host.to_ascii_lowercase(), path))
}

/// Who may write to a receiver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admit {
    /// Nobody: every write is refused (the freeze, and the lock until the
    /// mirror key exists).
    Nobody,
    /// The mirror principal as a user, bot or API identity of the receiver.
    User(String),
    /// A mirror key, enrolled as a write deploy key. `title` names the key.
    Key { public_key: String, title: String },
}

/// How a sender reaches a receiver; produced by the receiver's adapter.
#[derive(Clone, Debug)]
pub struct Target {
    pub site: Site,
    /// Clone URL over HTTPS, without credentials.
    pub https: String,
    /// Clone URL a mirror key can use, when the receiver accepts one.
    pub key_url: Option<String>,
    /// Login a sender stores next to the credential.
    pub login: String,
    /// Environment variable that holds the credential of `principal`.
    pub secret_env: String,
    /// Identity the receiver admits for a credential-based mirror.
    pub principal: String,
}

/// A push mirror a site sends, as its forge reports it.
#[derive(Clone, Debug)]
pub struct Mirror {
    /// Forge-native identity (Forgejo `remote_name`, GitLab mirror id).
    pub id: String,
    /// Normalised target, see [`address_of`].
    pub address: String,
    /// Public half of the generated mirror key, when it authenticates by key.
    pub public_key: Option<String>,
    pub enabled: bool,
    /// Last run: `Some(true)` fine, `Some(false)` failed, `None` never ran.
    pub healthy: Option<bool>,
}

/// Peeled commit id of every branch and tag.
pub type Refs = BTreeMap<String, String>;

/// The credential named by an environment reference: a nonempty run of
/// printable ASCII, never echoed.
pub fn secret(env: &str) -> Result<String> {
    crate::native::env_name(env)?;
    let value = std::env::var(env)
        .map_err(|_| failure("Missing mirror credential environment reference"))?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(failure("Invalid mirror credential format"));
    }
    Ok(value)
}

/// Ownership records: only mirrors cfrg recorded (or the placement declared)
/// are ever disabled or replaced. Implemented by the native request state.
pub trait Ledger {
    fn owned(&self, key: &str) -> Option<String>;
    fn record(&mut self, key: &str, value: &str) -> Result<()>;
    fn forget(&mut self, key: &str) -> Result<()>;
}

fn unsupported(forge: Forge, what: &str) -> crate::Result<()> {
    Err(crate::failure(format!(
        "{} cannot {what}; the declared capability says so (cfrg switch --capabilities)",
        forge.as_str()
    )))
}

/// One repository of a replica set, as an adapter exposes it. Everything a
/// forge-specific rule decides lives behind this trait; the procedures in
/// [`crate::switch`] never name a forge. Reads and writes go through the
/// caller's transport, so rate windows, pacing and uncertain-write holds are
/// shared by every adapter.
pub trait Replica {
    fn site(&self) -> &Site;
    fn capabilities(&self) -> Capabilities;
    /// Read the repository and check identity and visibility against the placement.
    fn verify(&self, io: &mut dyn Transport) -> Result<()>;
    fn refs(&self, io: &mut dyn Transport) -> Result<Refs>;
    /// How a sender reaches this repository.
    fn target(&self) -> Result<Target>;
    /// Push mirrors this repository sends, owned or not.
    fn mirrors(&self, _io: &mut dyn Transport) -> Result<Vec<Mirror>> {
        Ok(Vec::new())
    }
    fn add_mirror(&self, _io: &mut dyn Transport, _to: &Target) -> Result<Mirror> {
        unsupported(self.site().forge, "send a native push mirror")?;
        unreachable!("unsupported always returns an error")
    }
    fn remove_mirror(&self, _io: &mut dyn Transport, _mirror: &Mirror) -> Result<()> {
        unsupported(self.site().forge, "remove a push mirror")
    }
    /// Ask the forge to run the mirror now (best effort; it also runs by itself).
    fn run_mirror(&self, _io: &mut dyn Transport, _mirror: &Mirror) -> Result<()> {
        Ok(())
    }
    /// Receiver protection: only `admit` may write. Idempotent.
    fn lock(&self, io: &mut dyn Transport, admit: &Admit) -> Result<()>;
    /// Restore the repository for normal landing. Idempotent.
    fn unlock(&self, io: &mut dyn Transport) -> Result<()>;
}

/// Plain JSON of the capability table, one row per forge.
pub fn table(rows: &[Capabilities]) -> Value {
    let mut table = serde_json::Map::new();
    for row in rows {
        table.insert(
            row.forge.as_str().into(),
            serde_json::to_value(row).unwrap_or(Value::Null),
        );
    }
    Value::Object(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn cap(support: Support) -> Capability {
        Capability { support, note: "" }
    }
    fn caps(push: Support, pull: Support, converts: bool) -> Capabilities {
        Capabilities {
            forge: Forge::Forgejo,
            push_mirror: cap(push),
            mirror_key: false,
            pull_mirror: cap(pull),
            pull_mirror_converts_existing: converts,
            receiver_lock: cap(Support::Native),
            switch: cap(Support::Native),
            rename: cap(Support::Native),
        }
    }

    #[test]
    fn source_push_mirror_beats_destination_pull_mirror_beats_sync() {
        let push = caps(Support::Native, Support::Unsupported, false);
        let pull_new_only = caps(Support::AdapterOnly, Support::NativeFill, false);
        let nothing = caps(Support::AdapterOnly, Support::Unsupported, false);
        assert_eq!(
            choose(&push, &pull_new_only, true),
            Method::SourcePushMirror
        );
        assert_eq!(
            choose(&nothing, &pull_new_only, false),
            Method::DestinationPullMirror
        );
        // An existing repository cannot become a pull mirror on this forge.
        assert_eq!(choose(&nothing, &pull_new_only, true), Method::Sync);
        let pull_existing = caps(Support::AdapterOnly, Support::Native, true);
        assert_eq!(
            choose(&nothing, &pull_existing, true),
            Method::DestinationPullMirror
        );
        assert_eq!(choose(&nothing, &nothing, true), Method::Sync);
    }

    #[test]
    fn mirror_addresses_ignore_scheme_credentials_and_suffix() {
        for url in [
            "https://*****:*****@forge.corbet.ch/corbet-media/switch-probe.git",
            "ssh://git@forge.corbet.ch/corbet-media/switch-probe.git",
            "https://user:secret@FORGE.corbet.ch/corbet-media/switch-probe",
            "forge.corbet.ch/corbet-media/switch-probe/",
        ] {
            assert_eq!(
                address_of(url).as_deref(),
                Some("forge.corbet.ch/corbet-media/switch-probe"),
                "{url}"
            );
        }
        assert_eq!(address_of("https://host"), None);
        assert_eq!(address_of("https://host/"), None);
    }

    #[test]
    fn ownership_key_keeps_the_native_forgejo_form() {
        let source = Site::new(
            Forge::Forgejo,
            "https://forge.example",
            "https://forge.example",
            "forge.example",
            "team/project",
            Some("123".into()),
        );
        let gitlab = Site::new(
            Forge::Gitlab,
            "https://gitlab.example",
            "https://gitlab.example",
            "gitlab.example",
            "team/project",
            None,
        );
        assert_eq!(
            source.mirror_key(&gitlab),
            "https://forge.example/123/https://gitlab.example/team/project"
        );
        assert_eq!(
            gitlab.mirror_key(&source),
            "https://gitlab.example/team/project/https://forge.example/team/project"
        );
    }
}
