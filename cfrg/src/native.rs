//! Declarative native replication control plane. No Git data transfer here.
use crate::{failure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub mod derive;
pub mod http;
pub mod links;
pub mod settings;
pub mod transfer;
pub mod wiki;
pub mod wiki_sync;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub schema: u32,
    pub source: Endpoint,
    /// How a sender writes into the Forgejo source once it is a receiver.
    #[serde(default)]
    pub receiver: Option<Receiver>,
    /// Base URL of the default primary (the git-pointer view of the placement).
    #[serde(default)]
    pub default: Option<String>,
    /// Lowercase `owner/repo` to the base URL of its primary, exceptions only.
    #[serde(default)]
    pub primaries: BTreeMap<String, String>,
    pub repositories: Vec<Repository>,
    /// A plain git host over SSH that receives EVERY repository the source lists
    /// (and its wiki): a policy over the forge's own listing, not a list of
    /// repositories, so a new repository is covered without a placement edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<Copy>,
    /// Someone else's projects that we contribute to (operating model R10, R11): never mirrored, relayed or released
    /// by us, and their copies on our forge run no Actions. See `docs/contributed.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributed: Vec<ContributedProject>,
    /// The policy that derives the default copies of every listed repository that is not declared above (see
    /// `derive`). `repositories` then keeps only real exceptions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derive: Option<derive::Derive>,
    /// Lowercase `owner/repo` to `provider` to `pointers-only`: the repositories (LFS) whose copy on that forge is declared to carry
    /// the pointer files only (R4, R64: the payloads stay on the primary). The same declaration the content replication reads
    /// (infra lib/placement.json `external_content`). Without it an LFS repository is an exception that native makes no copy of.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub external_content: BTreeMap<String, BTreeMap<String, String>>,
    /// Lowercase Forgejo organisation to provider to the namespace that organisation has there (GitLab group, Bitbucket
    /// workspace, GitHub owner). The one place a move between namespaces is declared: change the Forgejo organisation of a
    /// repository, or change this map, and every destination that is not pinned follows (a transfer, see `transfer`).
    /// A provider without an entry keeps the organisation's own name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub org_map: OrgMap,
}

/// Forgejo organisation (lowercase) to provider to the namespace on that provider.
pub type OrgMap = BTreeMap<String, BTreeMap<Provider, String>>;

impl Placement {
    /// Whether native may mirror at least one destination of this repository. native-git: all; an LFS repository: the destinations that are
    /// themselves an SSH push mirror (history only, so the copy can only carry pointers) declared `pointers-only` for their forge. A
    /// profile repository, a hold, or a hidden-name repository stays an exception.
    pub fn mirrorable(&self, repo: &Repository) -> bool {
        if repo.hold.is_some()
            || repo
                .path
                .rsplit('/')
                .next()
                .is_some_and(|p| p.starts_with('.'))
        {
            return false;
        }
        match repo.content {
            Content::NativeGit => true,
            Content::Profile => false,
            Content::Lfs => repo
                .destinations
                .iter()
                .any(|d| self.destination_mirrorable(repo, d)),
        }
    }

    /// Whether native may mirror this destination of the repository: always for native-git; for LFS only when the destination is an
    /// SSH push mirror and `external_content` declares `pointers-only` for its forge. A destination of an LFS repository that fails the
    /// test (full LFS content, carried by the content replication) is left alone, whatever the other destinations are.
    pub fn destination_mirrorable(&self, repo: &Repository, dest: &Destination) -> bool {
        match repo.content {
            Content::NativeGit => true,
            Content::Profile => false,
            Content::Lfs => dest.use_ssh && self.pointers_only_on(repo, dest.provider),
        }
    }

    /// Whether `external_content` declares this repository's copy on the forge `pointers-only`.
    pub fn pointers_only_on(&self, repo: &Repository, provider: Provider) -> bool {
        let forge = match provider {
            Provider::Gitlab => "gitlab",
            Provider::Bitbucket => "bitbucket",
            Provider::Github => "github",
        };
        self.external_content
            .get(&repo.path.to_ascii_lowercase())
            .and_then(|m| m.get(forge))
            .map(String::as_str)
            == Some("pointers-only")
    }
}

/// One contributed project: its own repository and the other places the same project is found (our copies, a second home).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContributedProject {
    /// A plain lowercase name, unique; the list is in strictly ascending order.
    pub name: String,
    /// `https://host/owner/repo` of the project itself.
    pub upstream: String,
    /// Other locations of the same project: our copies on any forge (ours under every host spelling it is cloned by), or a second
    /// home of the project itself (the Tor project's own repository beside the fork we follow).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forks: Vec<String>,
}

/// The receiver of the copy. Forgejo's own SSH push mirrors do the transfer; cfrg
/// provisions the bare repositories and admits each mirror's public key through
/// the receiver's admin channel (docs/native-mirrors.md, "The copy").
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Copy {
    /// `host` or `host:port` of the receiver's SSH server, as the forge reaches it.
    pub host: String,
    /// Login of the receiving account, never an email.
    pub user: String,
    /// Environment reference naming a file with cfrg's private key for the admin channel.
    pub admin_key_env: String,
    /// Environment reference naming the known_hosts file that pins the receiver's host key.
    pub known_hosts_env: String,
    pub interval_seconds: u64,
}

/// The identity senders write as when Forgejo is a receiver, and the
/// environment reference holding its (narrowly scoped) credential.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Receiver {
    pub mirror_user: String,
    pub password_env: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub origin: String,
    pub token_env: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub path: String,
    pub source_id: u64,
    pub private: bool,
    pub default_branch: String,
    pub content: Content,
    /// Positive content/identity exceptions stay declared, never silently skipped.
    pub hold: Option<String>,
    pub destinations: Vec<Destination>,
    /// The placement's `org_map`, attached on loading; not part of the file format.
    #[serde(skip)]
    pub org_map: std::sync::Arc<OrgMap>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub provider: Provider,
    pub endpoint: Endpoint,
    /// Where the destination lives. Without `path_reason` the destination follows
    /// the primary's current `<org>/<repo>` and this only records the last known
    /// name (it may be left out); with a reason it is pinned to exactly this path.
    #[serde(default)]
    pub path: String,
    /// Why this destination deliberately keeps a name other than the primary's.
    /// Only a declared reason pins `path`; without one the destination is renamed
    /// whenever the primary is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_reason: Option<String>,
    /// The name as written in the file, before the destination followed its
    /// primary. Set on loading; not part of the file format.
    #[serde(skip)]
    pub recorded_path: String,
    /// Namespace ID for GitLab; existing project key for Bitbucket; the owner
    /// (organisation) login for GitHub.
    pub namespace: String,
    /// Immutable destination identity once known. Required for existing repos.
    pub repository_id: Option<String>,
    pub mirror_user: String,
    /// Bitbucket only: the user the branch restrictions admit (a user uuid). Without it the restrictions admit `mirror_user`,
    /// which is right only while the mirror logs in as that user; a mirror that logs in as `x-token-auth` declares the uuid here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restriction_user: Option<String>,
    pub password_env: String,
    #[serde(default)]
    pub use_ssh: bool,
    pub interval_seconds: u64,
    /// Whether Forgejo pushes at every commit (the default) or only at the interval. A destination that is paced
    /// by a gate (GitHub) sets `false`: the interval coalesces any number of commits into one push.
    #[serde(default = "sync_on_commit_default")]
    pub sync_on_commit: bool,
    #[serde(default)]
    pub branch_filter: String,
    pub hold: Option<String>,
    #[serde(default)]
    pub absent: bool,
    /// Explicit ownership claim, also needed for declarative deletion.
    pub remote_name: Option<String>,
    /// Declared exception: the receiver cannot be locked on this destination
    /// (GitHub Free has no rulesets on private repositories). The value is the
    /// reason. Only a private GitHub destination may declare it; the mirror is
    /// then configured without the receiver lock and the pass reports `lock`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_exception: Option<String>,
    // There is deliberately no `ci` field (operating model R6, R7, R9, R42): no copy can declare its provider's CI
    // (GitHub Actions, GitLab CI, Bitbucket Pipelines) or Dependabot on, and the reconcile always switches them off.
    // `deny_unknown_fields` makes an old `"ci": ...` in a placement file a load error, so the declaration fails closed.
}

fn sync_on_commit_default() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Gitlab,
    Bitbucket,
    Github,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Content {
    NativeGit,
    Lfs,
    Profile,
}

impl Destination {
    /// The user a Bitbucket branch restriction admits.
    pub fn restriction_user(&self) -> &str {
        self.restriction_user
            .as_deref()
            .unwrap_or(&self.mirror_user)
    }

    /// Only a declared reason pins the path; every other destination follows the
    /// primary's name.
    pub fn pinned(&self) -> bool {
        !self.path.is_empty()
            && self
                .path_reason
                .as_deref()
                .is_some_and(|reason| !reason.trim().is_empty())
    }
}

impl Repository {
    /// Where `dest` must live now: its pinned path, else the primary's current
    /// `<org>/<repo>` (this repository's `path`).
    pub fn destination_path(&self, dest: &Destination) -> String {
        if dest.pinned() {
            dest.path.clone()
        } else {
            self.mapped_path(dest.provider)
        }
    }

    /// Whether the placement declares, in `org_map`, a namespace for this repository's organisation on `provider`. Only then
    /// is a namespace change of its destination moved automatically; without it the change is reported `transfer-needed` and
    /// nothing moves implicitly.
    pub fn transfer_declared(&self, provider: Provider) -> bool {
        self.path
            .split_once('/')
            .and_then(|(org, _)| self.org_map.get(&org.to_ascii_lowercase()))
            .is_some_and(|targets| targets.contains_key(&provider))
    }

    /// The primary's path with its organisation replaced by what `org_map` says for `provider`.
    pub fn mapped_path(&self, provider: Provider) -> String {
        let Some((org, rest)) = self.path.split_once('/') else {
            return self.path.clone();
        };
        match self
            .org_map
            .get(&org.to_ascii_lowercase())
            .and_then(|targets| targets.get(&provider))
        {
            Some(namespace) => format!("{namespace}/{rest}"),
            None => self.path.clone(),
        }
    }
}

/// How the actual name of a destination compares with the wanted one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Naming {
    /// The destination already has the wanted name (or has no recorded identity
    /// to look it up by).
    Current,
    /// It lives at `from` and would be renamed; nothing was written.
    Planned { from: String },
    /// It lived at `from` and was renamed by this call.
    Renamed { from: String },
    /// It lives at `from` and cannot be renamed; reported, never forced.
    Blocked { from: String, reason: &'static str },
}

/// What a rename of `live` to `wanted` would have to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Move<'a> {
    Same,
    /// Same namespace, other last component.
    Rename {
        leaf: &'a str,
    },
    /// The namespace differs: a transfer, which no adapter implements.
    Transfer,
}

pub fn plan_move<'a>(live: &str, wanted: &'a str) -> Move<'a> {
    if live == wanted {
        return Move::Same;
    }
    match (live.rsplit_once('/'), wanted.rsplit_once('/')) {
        (Some((live_ns, _)), Some((wanted_ns, leaf)))
            if live_ns.eq_ignore_ascii_case(wanted_ns) =>
        {
            Move::Rename { leaf }
        }
        _ => Move::Transfer,
    }
}

impl Placement {
    /// Read the placement from either view of the one declared file: the
    /// native view itself, or the whole `lib/placement.json` (its `native`
    /// object plus the top-level `default` and `primaries`).
    pub fn from_document(bytes: &[u8]) -> Result<Self> {
        let mut value: serde_json::Value = serde_json::from_slice(bytes)?;
        if let Some(mut native) = value.get_mut("native").map(serde_json::Value::take) {
            for key in ["default", "primaries"] {
                if let (Some(top), Some(object)) = (value.get(key), native.as_object_mut()) {
                    object.entry(key).or_insert_with(|| top.clone());
                }
            }
            value = native;
        }
        let mut placement: Self = serde_json::from_value(value)?;
        placement.follow_primaries();
        Ok(placement)
    }

    /// Keep the destination names as written and let every destination that is
    /// not pinned follow the declared name of its primary. `cfrg native` follows
    /// the primary's live name on top of this (a rename the file has not caught
    /// up with yet).
    fn follow_primaries(&mut self) {
        self.attach_org_map();
        for repo in &mut self.repositories {
            for dest in &mut repo.destinations {
                dest.recorded_path = dest.path.clone();
            }
            for index in 0..repo.destinations.len() {
                if !repo.destinations[index].pinned() {
                    repo.destinations[index].path =
                        repo.mapped_path(repo.destinations[index].provider);
                }
            }
        }
    }

    /// Hand every repository the `org_map` (also after `derive` added repositories).
    pub fn attach_org_map(&mut self) {
        let map = std::sync::Arc::new(self.org_map.clone());
        for repo in &mut self.repositories {
            repo.org_map = map.clone();
        }
    }

    /// Base URL of the primary of `path`: its exception, else the declared
    /// default, else the Forgejo source (every repository before a switch).
    pub fn primary_of(&self, path: &str) -> String {
        let url = self
            .primaries
            .get(&path.to_ascii_lowercase())
            .or(self.default.as_ref())
            .unwrap_or(&self.source.origin);
        url.trim_end_matches('/').to_string()
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != 1 {
            return Err(failure("Unsupported native placement schema"));
        }
        self.source.validate()?;
        for url in self.primaries.values().chain(self.default.iter()) {
            Endpoint {
                origin: url.clone(),
                token_env: "UNUSED".into(),
            }
            .validate()?;
        }
        if let Some(receiver) = &self.receiver {
            env_name(&receiver.password_env)?;
            if receiver.mirror_user.is_empty()
                || receiver.mirror_user.contains('@')
                || receiver.mirror_user.chars().any(char::is_control)
            {
                return Err(failure(
                    "Mirror principal must be an ID/login, never an email",
                ));
            }
        }
        if let Some(copy) = &self.copy {
            copy.validate()?;
        }
        if let Some(derive) = &self.derive {
            derive.validate()?;
        }
        for (org, targets) in &self.org_map {
            if org.is_empty() || org != &org.to_ascii_lowercase() || org.contains('/') {
                return Err(failure("org_map keys are lowercase organisation names"));
            }
            for (provider, namespace) in targets {
                // A GitLab group may be nested; a Bitbucket workspace or a GitHub owner is one component.
                path(&format!("{namespace}/x"), *provider == Provider::Gitlab)?;
            }
        }
        // The declaration of contributed projects, and that none of them has a mirror destination (R11).
        crate::contributed::Contributed::from_placement(self, &[])?;
        let mut sources = BTreeSet::new();
        let mut targets = BTreeSet::new();
        for repo in &self.repositories {
            path(&repo.path, false)?;
            if repo.source_id == 0 || !sources.insert(&repo.path) || repo.default_branch.is_empty()
            {
                return Err(failure("Missing or duplicate source identity"));
            }
            for dest in &repo.destinations {
                dest.endpoint.validate()?;
                // `path` was resolved on loading; the written one is `recorded_path`.
                match (&dest.path_reason, dest.recorded_path.is_empty()) {
                    (Some(reason), false)
                        if !reason.trim().is_empty() && !reason.chars().any(char::is_control) => {}
                    (None, _) => {}
                    _ => {
                        return Err(failure(
                            "A path reason needs a pinned path and must be plain text",
                        ))
                    }
                }
                path(&dest.path, dest.provider == Provider::Gitlab)?;
                if !targets.insert((&dest.endpoint.origin, &dest.path))
                    || dest.interval_seconds < 600
                {
                    return Err(failure(
                        "Duplicate destination or interval below ten minutes",
                    ));
                }
                env_name(&dest.password_env)?;
                if dest.provider == Provider::Github {
                    // GitHub is a mirror destination only, in an organisation that
                    // owns the path, reached through its one API origin.
                    let owner = dest.path.split('/').next().unwrap_or_default();
                    if dest.endpoint.origin != "https://api.github.com"
                        || !dest.namespace.eq_ignore_ascii_case(owner)
                    {
                        return Err(failure(
                            "GitHub destinations use https://api.github.com and name their owner as namespace",
                        ));
                    }
                }
                if let Some(reason) = &dest.lock_exception {
                    if dest.provider != Provider::Github
                        || !repo.private
                        || reason.trim().is_empty()
                        || reason.chars().any(char::is_control)
                    {
                        return Err(failure(
                            "A lock exception needs a private GitHub destination and a plain-text reason",
                        ));
                    }
                }
                if dest.use_ssh && !matches!(dest.provider, Provider::Gitlab | Provider::Bitbucket)
                {
                    return Err(failure(
                        "SSH enrollment requires GitLab deploy keys or a Bitbucket account key",
                    ));
                }
                if dest.mirror_user.is_empty()
                    || dest.mirror_user.contains('@')
                    || dest.mirror_user.chars().any(char::is_control)
                {
                    return Err(failure(
                        "Mirror principal must be an ID/login, never an email",
                    ));
                }
                if let Some(remote) = &dest.remote_name {
                    component(remote)?;
                }
                if dest.absent && dest.remote_name.is_none() {
                    return Err(failure("Deletion requires declared remote_name ownership"));
                }
            }
        }
        Ok(())
    }
}

impl Copy {
    pub fn validate(&self) -> Result<()> {
        env_name(&self.admin_key_env)?;
        env_name(&self.known_hosts_env)?;
        let (host, port) = self.host_and_port();
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
            || port.is_some_and(|p| p.is_empty() || p.parse::<u16>().is_err())
        {
            return Err(failure(
                "Copy host must be a plain host name or address with an optional port",
            ));
        }
        if self.user.is_empty()
            || !self
                .user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(failure("Copy user must be a login, never an email"));
        }
        if self.interval_seconds < 600 {
            return Err(failure("Copy interval below ten minutes"));
        }
        Ok(())
    }
    /// The host and the optional port text of `host[:port]`.
    pub fn host_and_port(&self) -> (&str, Option<&str>) {
        match self.host.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (&self.host, None),
        }
    }
    /// The address of a repository on the receiver as the API reports it (the
    /// account is sanitized away). `path` is `<owner>/<repo>`, lowercase.
    pub fn address(&self, path: &str) -> String {
        format!("ssh://{}/{path}.git", self.host)
    }
    /// The address handed to Forgejo when the mirror is created.
    pub fn push_address(&self, path: &str) -> String {
        format!("ssh://{}@{}/{path}.git", self.user, self.host)
    }
}

impl Endpoint {
    pub fn validate(&self) -> Result<()> {
        env_name(&self.token_env)?;
        let host = self
            .origin
            .strip_prefix("https://")
            .ok_or_else(|| failure("HTTPS origin required"))?;
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
        {
            return Err(failure(
                "Origin must contain only an HTTPS host and optional port",
            ));
        }
        Ok(())
    }
}

pub fn component(value: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(failure("Invalid repository component"));
    }
    Ok(())
}

pub fn path(value: &str, nested: bool) -> Result<()> {
    let parts: Vec<_> = value.split('/').collect();
    if parts.len() < 2 || (!nested && parts.len() != 2) {
        return Err(failure("Invalid repository path"));
    }
    for part in parts {
        component(part)?;
    }
    Ok(())
}

pub fn env_name(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(failure("Invalid credential environment reference"));
    }
    Ok(())
}

pub fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The one definition of ownership (design 3.7, R56): a push mirror that Forgejo holds for
/// `repo` is ours exactly when its address is a destination the placement declares for the
/// repository (under its wanted or its last recorded name), or an address on the git copy, or
/// it carries the `remote_name` the placement claims. Nothing is recorded anywhere. Any other
/// mirror is foreign: report it, never change it. Code that writes to a destination restricts
/// itself with this.
pub fn owned_by(repo: &Repository, copy: Option<&Copy>, mirror: &serde_json::Value) -> bool {
    let Some(address) = mirror["remote_address"]
        .as_str()
        .and_then(crate::replicate::address_of)
    else {
        return false;
    };
    if copy.is_some_and(|c| address.starts_with(&format!("{}/", c.host.to_ascii_lowercase()))) {
        return true;
    }
    repo.destinations.iter().any(|declared| {
        let host = match declared.provider {
            Provider::Gitlab => declared
                .endpoint
                .origin
                .split_once("://")
                .map_or(declared.endpoint.origin.as_str(), |(_, rest)| rest)
                .trim_end_matches('/')
                .to_ascii_lowercase(),
            Provider::Bitbucket => "bitbucket.org".to_string(),
            Provider::Github => "github.com".to_string(),
        };
        [
            repo.destination_path(declared),
            declared.recorded_path.clone(),
        ]
        .iter()
        .filter(|path| !path.is_empty())
        .any(|path| address.eq_ignore_ascii_case(&format!("{host}/{path}")))
            || declared
                .remote_name
                .as_deref()
                .is_some_and(|claim| mirror["remote_name"].as_str() == Some(claim))
    })
}

/// Ownership of a GitHub copy. GitHub copies are fed by the GitHub gate, never by a Forgejo push
/// mirror (one mechanism per forge), so the mirror test of [`owned_by`] cannot see them. The copy of
/// a (non-contributed, placed) primary sits at its declared GitHub destination, else at the
/// primary's own path; it is ours when the gate manages that `owner/repo` (a key of the gate
/// catalogue, `gate_repos`).
pub fn github_copy_owned(
    primary_path: &str,
    declared: Option<&Destination>,
    gate_repos: &[String],
) -> bool {
    let path = match declared {
        Some(d) if d.provider == Provider::Github && d.pinned() => d.path.clone(),
        Some(d) if d.provider == Provider::Github => primary_path.to_string(),
        Some(_) => return false,
        None => primary_path.to_string(),
    };
    gate_repos.iter().any(|g| g.eq_ignore_ascii_case(&path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn both_views_of_the_one_placement_file_load_and_name_the_primary() {
        let native = r#"{"schema":1,"source":{"origin":"https://forge.example","token_env":"T"},"repositories":[]}"#;
        let whole = r#"{"version":1,"default":"https://forge.example","primaries":{"team/probe":"https://gitlab.example"},
            "native":{"schema":1,"source":{"origin":"https://forge.example","token_env":"T"},"repositories":[]}}"#;
        let plain = Placement::from_document(native.as_bytes()).unwrap();
        assert_eq!(plain.primary_of("any/repo"), "https://forge.example");
        let placement = Placement::from_document(whole.as_bytes()).unwrap();
        placement.validate().unwrap();
        assert_eq!(placement.primary_of("Team/Probe"), "https://gitlab.example");
        assert_eq!(placement.primary_of("team/other"), "https://forge.example");
    }

    #[test]
    fn identities_cannot_change_request_authority() {
        for bad in ["a/../b", "a/x?token=x", "a/%2fsecret", "a/b#c", "a/b\\c"] {
            assert!(path(bad, true).is_err());
        }
        for bad in [
            "https://user@host",
            "https://host/path",
            "http://host",
            "https://host?secret",
        ] {
            assert!(Endpoint {
                origin: bad.into(),
                token_env: "TOKEN".into()
            }
            .validate()
            .is_err());
        }
        assert_eq!(encode("a/b*"), "a%2Fb%2A");
    }
    fn followers_document(extra: &str) -> String {
        format!(
            r#"{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"repositories":[
            {{"path":"team/new","source_id":1,"private":false,"default_branch":"main","content":"native-git","hold":null,"destinations":[
              {{"provider":"gitlab","endpoint":{{"origin":"https://gitlab.example","token_env":"G"}},"path":"team/old","namespace":"2","repository_id":"7","mirror_user":"9","password_env":"G","interval_seconds":3600,"hold":null,"remote_name":null}},
              {{"provider":"gitlab","endpoint":{{"origin":"https://gitlab.other","token_env":"G"}},"path":"archive/keep","path_reason":"the archive keeps the original name","namespace":"3","mirror_user":"9","password_env":"G","interval_seconds":3600,"hold":null,"remote_name":null}},
              {{"provider":"bitbucket","endpoint":{{"origin":"https://api.bitbucket.org","token_env":"B"}},"namespace":"CORE","mirror_user":"{{u}}","password_env":"B","interval_seconds":3600,"hold":null,"remote_name":null{extra}}}
            ]}}]}}"#
        )
    }

    #[test]
    fn ssh_push_mirrors_are_for_gitlab_and_bitbucket_only() {
        let doc = |provider: &str, origin: &str| {
            format!(
                r#"{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"repositories":[
                {{"path":"team/new","source_id":1,"private":false,"default_branch":"main","content":"native-git","hold":null,"destinations":[
                  {{"provider":"{provider}","endpoint":{{"origin":"{origin}","token_env":"G"}},"path":"team/new","namespace":"CORE","repository_id":null,"mirror_user":"{{u}}","password_env":"G","use_ssh":true,"interval_seconds":3600,"hold":null,"remote_name":null}}
                ]}}]}}"#
            )
        };
        let ok = |provider: &str, origin: &str| {
            Placement::from_document(doc(provider, origin).as_bytes())
                .unwrap()
                .validate()
        };
        assert!(ok("bitbucket", "https://api.bitbucket.org").is_ok());
        assert!(ok("github", "https://api.github.com").is_err());
    }

    #[test]
    fn destinations_follow_the_primary_unless_pinned_with_a_reason() {
        let placement = Placement::from_document(followers_document("").as_bytes()).unwrap();
        placement.validate().unwrap();
        let repo = &placement.repositories[0];
        let [renamed_by_file, pinned, left_out] = &repo.destinations[..] else {
            panic!("three destinations")
        };
        // A written name without a reason is only a record of the last known name.
        assert_eq!(renamed_by_file.path, "team/new");
        assert_eq!(renamed_by_file.recorded_path, "team/old");
        assert!(!renamed_by_file.pinned());
        assert_eq!(pinned.path, "archive/keep");
        assert!(pinned.pinned());
        assert_eq!(
            (left_out.path.as_str(), left_out.recorded_path.as_str()),
            ("team/new", "")
        );
        // The primary moves on: followers go with it, the pinned one stays.
        let mut live = repo.clone();
        live.path = "team/newer".into();
        assert_eq!(live.destination_path(renamed_by_file), "team/newer");
        assert_eq!(live.destination_path(left_out), "team/newer");
        assert_eq!(live.destination_path(pinned), "archive/keep");
    }

    #[test]
    fn a_path_reason_needs_a_pinned_path_and_plain_text() {
        for bad in [
            r#","path_reason":"why""#,
            r#","path":"team/x","path_reason":"""#,
            r#","path":"team/x","path_reason":"   ""#,
            r#","path":"team/x","path_reason":"bad\u0007""#,
        ] {
            let placement = Placement::from_document(followers_document(bad).as_bytes()).unwrap();
            assert!(placement.validate().is_err(), "{bad}");
        }
        let good = r#","path":"team/x","path_reason":"kept for the archive""#;
        let placement = Placement::from_document(followers_document(good).as_bytes()).unwrap();
        placement.validate().unwrap();
        assert!(placement.repositories[0].destinations[2].pinned());
    }

    #[test]
    fn a_github_destination_names_its_owner_and_its_one_api_origin() {
        let document = |origin: &str, namespace: &str| {
            format!(
                r#"{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"repositories":[
                {{"path":"team/repo","source_id":1,"private":false,"default_branch":"main","content":"native-git","hold":null,"destinations":[
                  {{"provider":"github","endpoint":{{"origin":"{origin}","token_env":"G"}},"namespace":"{namespace}","repository_id":"7","mirror_user":"bot","password_env":"G","interval_seconds":3600,"hold":null,"remote_name":null}}
                ]}}]}}"#
            )
        };
        let good = Placement::from_document(document("https://api.github.com", "Team").as_bytes())
            .unwrap();
        good.validate().unwrap();
        assert_eq!(
            good.repositories[0].destinations[0].provider,
            Provider::Github
        );
        for bad in [
            document("https://github.com", "team"),
            document("https://api.github.com", "other"),
        ] {
            let placement = Placement::from_document(bad.as_bytes()).unwrap();
            assert!(placement.validate().is_err());
        }
    }

    #[test]
    fn a_lock_exception_is_only_for_private_github_destinations_with_a_reason() {
        let document = |provider: &str, private: bool, extra: &str| {
            let (origin, namespace) = if provider == "github" {
                ("https://api.github.com", "team")
            } else {
                ("https://gitlab.example", "2")
            };
            format!(
                r#"{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"repositories":[
                {{"path":"team/repo","source_id":1,"private":{private},"default_branch":"main","content":"native-git","hold":null,"destinations":[
                  {{"provider":"{provider}","endpoint":{{"origin":"{origin}","token_env":"G"}},"namespace":"{namespace}","repository_id":"7","mirror_user":"bot","password_env":"G","interval_seconds":3600,"hold":null,"remote_name":null{extra}}}
                ]}}]}}"#
            )
        };
        let reason = r#","lock_exception":"GitHub Free cannot lock private repositories""#;
        let good = Placement::from_document(document("github", true, reason).as_bytes()).unwrap();
        good.validate().unwrap();
        assert!(good.repositories[0].destinations[0]
            .lock_exception
            .is_some());
        let none = Placement::from_document(document("github", true, "").as_bytes()).unwrap();
        none.validate().unwrap();
        assert!(none.repositories[0].destinations[0]
            .lock_exception
            .is_none());
        for bad in [
            document("github", false, reason),
            document("gitlab", true, reason),
            document("github", true, r#","lock_exception":"  ""#),
            document("github", true, r#","lock_exception":"bad\u0007""#),
        ] {
            let placement = Placement::from_document(bad.as_bytes()).unwrap();
            assert!(placement.validate().is_err());
        }
    }

    #[test]
    fn the_copy_is_one_policy_with_a_plain_host_a_login_and_a_floor_interval() {
        let document = |copy: &str| {
            format!(
                r#"{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"repositories":[],"copy":{copy}}}"#
            )
        };
        let good = r#"{"host":"192.0.2.6","user":"mirror","admin_key_env":"K","known_hosts_env":"H","interval_seconds":600}"#;
        let placement = Placement::from_document(document(good).as_bytes()).unwrap();
        placement.validate().unwrap();
        let copy = placement.copy.unwrap();
        assert_eq!(copy.address("org/repo"), "ssh://192.0.2.6/org/repo.git");
        assert_eq!(
            copy.push_address("org/repo"),
            "ssh://mirror@192.0.2.6/org/repo.git"
        );
        for bad in [
            good.replace("192.0.2.6", "srv/evil"),
            good.replace("192.0.2.6", "srv:"),
            good.replace("192.0.2.6", "srv:99999"),
            good.replace("mirror", "a@b"),
            good.replace("600", "599"),
            good.replace("\"K\"", "\"k\""),
            good.replace("}", ",\"extra\":1}"),
        ] {
            let result = Placement::from_document(document(&bad).as_bytes());
            assert!(
                result.is_err() || result.unwrap().validate().is_err(),
                "{bad}"
            );
        }
        let with_port = good.replace("192.0.2.6", "192.0.2.6:2222");
        let placement = Placement::from_document(document(&with_port).as_bytes()).unwrap();
        placement.validate().unwrap();
        assert_eq!(
            placement.copy.unwrap().host_and_port(),
            ("192.0.2.6", Some("2222"))
        );
    }

    #[test]
    fn a_move_is_a_rename_only_inside_one_namespace() {
        assert_eq!(plan_move("a/x", "a/x"), Move::Same);
        assert_eq!(plan_move("a/x", "a/y"), Move::Rename { leaf: "y" });
        assert_eq!(plan_move("A/x", "a/y"), Move::Rename { leaf: "y" });
        assert_eq!(plan_move("g/s/x", "g/s/y"), Move::Rename { leaf: "y" });
        assert_eq!(plan_move("a/x", "b/x"), Move::Transfer);
        assert_eq!(plan_move("g/s/x", "g/x"), Move::Transfer);
    }

    #[test]
    fn a_bitbucket_restriction_admits_the_declared_user_else_the_mirror_login() {
        let base = GITLAB.replace("\"gitlab\"", "\"bitbucket\"");
        let plain = declared_repo(&base);
        assert_eq!(plain.destinations[0].restriction_user(), "9");
        let declared = base.replace(
            "\"mirror_user\":\"9\"",
            "\"mirror_user\":\"x-token-auth\",\"restriction_user\":\"{uuid}\"",
        );
        let repo = declared_repo(&declared);
        assert_eq!(repo.destinations[0].mirror_user, "x-token-auth");
        assert_eq!(repo.destinations[0].restriction_user(), "{uuid}");
    }

    fn mapped_document(map: &str) -> String {
        format!(
            r#"{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"org_map":{map},"repositories":[{{"path":"web/site","source_id":5,"private":false,"default_branch":"main","content":"native-git","hold":null,"destinations":[
                {{"provider":"gitlab","endpoint":{{"origin":"https://gitlab.example","token_env":"T"}},"namespace":"2","repository_id":"7","mirror_user":"9","password_env":"T","interval_seconds":3600,"hold":null,"remote_name":null}},
                {{"provider":"bitbucket","endpoint":{{"origin":"https://api.bitbucket.org","token_env":"T"}},"namespace":"CORE","repository_id":"{{u}}","mirror_user":"9","password_env":"T","interval_seconds":3600,"hold":null,"remote_name":null}}
            ]}}]}}"#
        )
    }

    #[test]
    fn the_org_map_decides_the_namespace_per_provider() {
        let doc = mapped_document(r#"{"web":{"gitlab":"web-group/sub","bitbucket":"archv"}}"#);
        let placement = Placement::from_document(doc.as_bytes()).unwrap();
        placement.validate().unwrap();
        let repo = &placement.repositories[0];
        assert_eq!(repo.destinations[0].path, "web-group/sub/site");
        assert_eq!(repo.destinations[1].path, "archv/site");
        assert_eq!(
            repo.destination_path(&repo.destinations[0]),
            "web-group/sub/site"
        );
        // The primary moves to another organisation: the destination's namespace follows from the map.
        let mut live = repo.clone();
        live.path = "other/site".into();
        assert_eq!(live.destination_path(&live.destinations[1]), "other/site");
        assert_eq!(
            plan_move("web-group/sub/site", "other/site"),
            Move::Transfer
        );
        // Only a declared organisation moves automatically.
        assert!(repo.transfer_declared(Provider::Gitlab));
        assert!(!repo.transfer_declared(Provider::Github));
        let mut other = repo.clone();
        other.path = "unlisted/site".into();
        assert!(!other.transfer_declared(Provider::Gitlab));
        // No entry for an organisation: the destinations keep its own name.
        let plain = Placement::from_document(mapped_document("{}").as_bytes()).unwrap();
        assert_eq!(plain.repositories[0].destinations[0].path, "web/site");
    }

    #[test]
    fn a_bad_org_map_is_refused() {
        for map in [
            r#"{"Web":{"gitlab":"g"}}"#,
            r#"{"web":{"bitbucket":"a/b"}}"#,
            r#"{"web":{"github":""}}"#,
        ] {
            let placement = Placement::from_document(mapped_document(map).as_bytes()).unwrap();
            assert!(placement.validate().is_err(), "{map}");
        }
    }

    fn declared_repo(destinations: &str) -> Repository {
        serde_json::from_str(&format!(
            r#"{{"path":"team/new","source_id":5,"private":true,"default_branch":"main","content":"native-git","destinations":[{destinations}]}}"#
        ))
        .unwrap()
    }
    const GITLAB: &str = r#"{"provider":"gitlab","endpoint":{"origin":"https://gitlab.example","token_env":"T"},"path":"team/new","namespace":"2","repository_id":"7","mirror_user":"9","password_env":"T","use_ssh":true,"interval_seconds":3600}"#;

    #[test]
    fn a_github_copy_is_owned_when_the_gate_manages_its_path() {
        let github = GITLAB.replace("\"gitlab\"", "\"github\"");
        let repo = declared_repo(&github);
        let declared = &repo.destinations[0];
        let gate = vec!["Team/New".to_string(), "other/x".to_string()];
        assert!(github_copy_owned("team/new", Some(declared), &gate));
        assert!(!github_copy_owned(
            "team/new",
            Some(declared),
            &["other/x".to_string()]
        ));
        assert!(!github_copy_owned("team/new", Some(declared), &[]));
        // No declared GitHub destination: the copy sits at the primary's path.
        assert!(github_copy_owned("team/new", None, &gate));
        assert!(!github_copy_owned("team/else", None, &gate));
        // A declared destination on another forge says nothing about GitHub.
        let gitlab = declared_repo(GITLAB);
        assert!(!github_copy_owned(
            "team/new",
            Some(&gitlab.destinations[0]),
            &gate
        ));
    }

    #[test]
    fn a_mirror_is_owned_when_its_address_is_a_declared_destination_or_the_copy() {
        let mirror =
            |name: &str, address: &str| json!({"remote_name":name,"remote_address":address});
        let copy: Copy = serde_json::from_str(r#"{"host":"192.0.2.6","user":"git","admin_key_env":"K","known_hosts_env":"H","interval_seconds":600}"#).unwrap();
        let repo = declared_repo(GITLAB);
        // The declared destination, with or without the SSH userinfo Forgejo sanitises.
        assert!(owned_by(
            &repo,
            None,
            &mirror("a", "ssh://gitlab.example/team/new.git")
        ));
        assert!(owned_by(
            &repo,
            None,
            &mirror("a", "ssh://git@gitlab.example/Team/New.git")
        ));
        // Another path, another host or no address: foreign.
        assert!(!owned_by(
            &repo,
            None,
            &mirror("b", "ssh://gitlab.example/team/other.git")
        ));
        assert!(!owned_by(
            &repo,
            None,
            &mirror("c", "https://bitbucket.org/team/new.git")
        ));
        assert!(!owned_by(&repo, None, &json!({"remote_name":"d"})));
        // The git copy is ours only when the placement declares one.
        let on_copy = mirror("e", "ssh://192.0.2.6/team/new.git");
        assert!(!owned_by(&repo, None, &on_copy));
        assert!(owned_by(&repo, Some(&copy), &on_copy));
        assert!(!owned_by(
            &repo,
            Some(&copy),
            &mirror("f", "ssh://192.0.2.7/team/new.git")
        ));
        // A declared remote_name is an explicit claim.
        let claimed = declared_repo(
            &GITLAB.replace(r#""use_ssh":true"#, r#""use_ssh":true,"remote_name":"g""#),
        );
        assert!(owned_by(
            &claimed,
            None,
            &mirror("g", "ssh://gitlab.example/elsewhere.git")
        ));
    }

    #[test]
    fn an_lfs_repository_is_mirrorable_only_when_every_destination_is_an_ssh_mirror_declared_pointers_only(
    ) {
        let doc = |content: &str,
                   hold: &str,
                   gitlab_ssh: bool,
                   bitbucket_ssh: bool,
                   declared: &str| {
            format!(
                r#"{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"external_content":{declared},"repositories":[{{"path":"o/r","source_id":1,"private":false,"default_branch":"main","content":"{content}","hold":{hold},"destinations":[
                {{"provider":"gitlab","endpoint":{{"origin":"https://gitlab.com","token_env":"G"}},"path":"o/r","namespace":"1","repository_id":null,"mirror_user":"1","password_env":"G","use_ssh":{gitlab_ssh},"interval_seconds":3600,"branch_filter":"","hold":null,"absent":false,"remote_name":null}},
                {{"provider":"bitbucket","endpoint":{{"origin":"https://api.bitbucket.org","token_env":"B"}},"path":"o/r","namespace":"PUB","repository_id":null,"mirror_user":"u","password_env":"B","use_ssh":{bitbucket_ssh},"interval_seconds":3600,"branch_filter":"","hold":null,"absent":false,"remote_name":null}}]}}]}}"#
            )
        };
        let both = r#"{"o/r":{"gitlab":"pointers-only","bitbucket":"pointers-only"}}"#;
        let only_gitlab = r#"{"o/r":{"gitlab":"pointers-only"}}"#;
        let mirrorable = |document: String| {
            let placement = Placement::from_document(document.as_bytes()).unwrap();
            placement.mirrorable(&placement.repositories[0])
        };
        // native-git: always
        assert!(mirrorable(doc("native-git", "null", false, false, "{}")));
        // LFS declared pointers-only on every forge it is copied to, both SSH push mirrors: mirrored like native-git
        assert!(mirrorable(doc("lfs", "null", true, true, both)));
        // LFS that is NOT declared stays an exception
        assert!(!mirrorable(doc("lfs", "null", true, true, "{}")));
        // declared for one forge only: that destination is mirrored, the other (real LFS content) is left to the content replication
        let one = Placement::from_document(doc("lfs", "null", true, true, only_gitlab).as_bytes())
            .unwrap();
        let repo = &one.repositories[0];
        assert!(one.mirrorable(repo));
        assert!(one.destination_mirrorable(repo, &repo.destinations[0]));
        assert!(!one.destination_mirrorable(repo, &repo.destinations[1]));
        // a destination that is not an SSH push mirror (it would push LFS objects) is never mirrored; none left: an exception
        assert!(mirrorable(doc("lfs", "null", true, false, both)));
        assert!(!mirrorable(doc("lfs", "null", false, false, both)));
        // a hold, or a profile repository, stays an exception whatever is declared
        assert!(!mirrorable(doc("lfs", "\"held\"", true, true, both)));
        assert!(!mirrorable(doc("profile", "null", true, true, both)));
    }
}
