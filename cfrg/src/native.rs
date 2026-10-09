//! Declarative native replication control plane. No Git data transfer here.
use crate::{failure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub mod http;

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
    pub password_env: String,
    #[serde(default)]
    pub use_ssh: bool,
    pub interval_seconds: u64,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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
            self.path.clone()
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
        for repo in &mut self.repositories {
            let current = repo.path.clone();
            for dest in &mut repo.destinations {
                dest.recorded_path = dest.path.clone();
                if !dest.pinned() {
                    dest.path = current.clone();
                }
            }
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
                if dest.use_ssh && dest.provider != Provider::Gitlab {
                    return Err(failure("SSH enrollment requires GitLab deploy keys"));
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

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(good.repositories[0].destinations[0].lock_exception.is_some());
        let none = Placement::from_document(document("github", true, "").as_bytes()).unwrap();
        none.validate().unwrap();
        assert!(none.repositories[0].destinations[0].lock_exception.is_none());
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
    fn a_move_is_a_rename_only_inside_one_namespace() {
        assert_eq!(plan_move("a/x", "a/x"), Move::Same);
        assert_eq!(plan_move("a/x", "a/y"), Move::Rename { leaf: "y" });
        assert_eq!(plan_move("A/x", "a/y"), Move::Rename { leaf: "y" });
        assert_eq!(plan_move("g/s/x", "g/s/y"), Move::Rename { leaf: "y" });
        assert_eq!(plan_move("a/x", "b/x"), Move::Transfer);
        assert_eq!(plan_move("g/s/x", "g/x"), Move::Transfer);
    }
}
