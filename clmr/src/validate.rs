//! Validated request types. Every constructor rejects malformed input; the
//! selector below only sees canonical values. No domain hardcoding: hosts and
//! identities arrive as Infra config data.
use crate::{failure, Error};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Maximum lengths keep validation bounded and error messages stable.
const MAX_URL: usize = 2048;
const MAX_PATH: usize = 256;
const MAX_SCOPES: usize = 256;

/// Typed resolve request. Field names are frozen by RESOLVER-API.md v1.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema: u32,
    /// Canonical pointer base, always HTTPS (e.g. the git pointer host).
    pub canonical_base: String,
    #[serde(default)]
    pub aliases: Vec<Alias>,
    pub repositories: Vec<Repository>,
    pub stores: Vec<Store>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Optional live primary feed (CLI enrichment only; see PrimarySource).
    /// A request-supplied per-repo `primary` always wins; the feed fills
    /// unknown primaries for moving refs only. Never both ambiguous: an
    /// explicit primary is authoritative, the feed is fallback.
    #[serde(default)]
    pub primary_source: Option<PrimarySource>,
}

fn default_timeout() -> u64 {
    30
}

/// Explicit owned-URL mapping: `url_prefix` normalizes to
/// `{canonical_base}/{canonical_owner}/...` with segment boundaries.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    pub url_prefix: String,
    pub canonical_owner: String,
}

/// Declared live primary feed: an unauthenticated redirect authority that
/// maps canonical repo paths to their primary base URL (see the git-pointer
/// Worker: `owner/repo` keys, exact base match, default Forgejo). The CLI
/// reads it with a bounded HEAD per moving repo with unknown primary; the
/// pure selector never sees it. Absent = no lookup (policy/null only).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrimarySource {
    /// Bare `http(s)` origin of the redirect authority (no path).
    pub pointer_base: String,
    /// Exact primary base origin -> opaque identity allowlist. A lookup
    /// Location outside these keys resolves to unknown, never Forgejo.
    pub identities: BTreeMap<String, String>,
    /// Per-lookup bound, seconds.
    #[serde(default = "default_lookup_timeout")]
    pub timeout_secs: u64,
}

fn default_lookup_timeout() -> u64 {
    10
}

/// One repo fetch need: canonical path, ref kind, optional primary identity.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub id: String,
    pub path: String,
    pub r#ref: RefKind,
    #[serde(default)]
    pub primary: Option<String>,
    /// Explicit Policy location of the primary (metadata only; never a
    /// fetch target — fallback is always the canonical pointer).
    #[serde(default)]
    pub primary_url: Option<String>,
    /// Every fetch URL form the job will use for this repo (bare canonical,
    /// `.git` canonical, owned-alias forms, ...). Deterministic and complete
    /// by construction: only declared forms are ever rewritten, each
    /// normalizing back to `path`. Undeclared neighbor URLs are NOT safely
    /// routable (git insteadOf is a raw prefix match); see render docs.
    pub source_urls: Vec<String>,
}

/// Pinned content hash or a moving full ref. Nothing else is addressable.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub enum RefKind {
    #[serde(rename = "pinned")]
    Pinned(String),
    #[serde(rename = "moving")]
    Moving(String),
}

/// One declared store. Order is per-runner config data (first hit wins).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Store {
    pub kind: StoreKind,
    pub location: String,
    pub identity: String,
    pub scope: Vec<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub credential_env: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub trusted_single_user: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreKind {
    HttpForge,
    Filesystem,
}

/// Credential reference attached to a response (never a value).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialScope {
    pub r#match: String,
    pub env: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

fn plain_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn plain_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}

fn valid_env_name(value: &str) -> bool {
    value.strip_prefix("CFRG_RESOLVER_").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    })
}

fn bad_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // Checked slice: a trailing `%` or `%X` is malformed, never panic.
            let Some(pair) = bytes.get(i + 1..i + 3) else {
                return true;
            };
            if !pair.iter().all(|b| b.is_ascii_hexdigit()) {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// Reject anything that must never reach git/curl argv or config rendering.
fn clean_url_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_URL
        && !value.bytes().any(|b| {
            b.is_ascii_control()
                || b" \t\"'`<>{}|\\^$;&()!*?".contains(&b)
                || b == b'['
                || b == b']'
        })
        && !bad_percent_encoding(value)
}

fn split_origin(value: &str) -> Option<(&str, &str)> {
    if let Some(rest) = value.strip_prefix("https://") {
        Some(("https", rest))
    } else if let Some(rest) = value.strip_prefix("http://") {
        Some(("http", rest))
    } else {
        None
    }
}

/// Port in canonical spelling: nonzero u16, no leading zeros. Hostname
/// case is preserved by validation; `normalize_origin` lowercases it so both
/// credential sides compare the same normalized authority.
fn parse_port(port: &str) -> Option<u16> {
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u16 = port.parse().ok()?;
    if value == 0 || value.to_string() != port {
        return None;
    }
    Some(value)
}

fn valid_host_port(host: &str) -> bool {
    let name = match host.split_once(':') {
        Some((name, port)) => {
            if parse_port(port).is_none() {
                return false;
            }
            name
        }
        None => host,
    };
    !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
}

/// Username safe for both git-config and `!shell` helper interpolation:
/// alphanumeric start (no leading dash/dot), narrow charset, no quoting
/// needed and none attempted.
pub fn valid_username(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Bare origin without userinfo/query/fragment/path; scheme per
/// `https_only`. Store locations and bases are origins only, so a location
/// IS the credential origin (no separate derivation needed).
fn valid_endpoint(value: &str, https_only: bool) -> Result<(), Error> {
    if !clean_url_text(value) {
        return Err(failure("Invalid endpoint characters or encoding"));
    }
    let (scheme, rest) = split_origin(value).ok_or_else(|| failure("Endpoint requires http(s)"))?;
    if https_only && scheme != "https" {
        return Err(failure("Endpoint requires HTTPS"));
    }
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if host.contains('@') || !valid_host_port(host) {
        return Err(failure("Invalid endpoint host"));
    }
    if !path.is_empty() && path != "/" {
        return Err(failure("Endpoint must be a bare origin without path"));
    }
    if path.contains('?') || path.contains('#') {
        return Err(failure("Endpoint forbids query and fragment"));
    }
    Ok(())
}

/// Owned-URL prefix (`https://github.com/acme`, ...): http(s) origin plus
/// explicit plain owner path segments. Unlike `valid_endpoint`, a path is
/// REQUIRED here (the prefix names the owned scope); unlike free URLs, every
/// segment must be plain ASCII with no traversal, userinfo, query,
/// fragment, empty segments, trailing slash, or `.git` form.
fn valid_alias_prefix(value: &str) -> Result<(), Error> {
    if !clean_url_text(value) {
        return Err(failure("Invalid alias prefix characters or encoding"));
    }
    let (_, rest) = split_origin(value).ok_or_else(|| failure("Alias prefix requires http(s)"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if host.contains('@') || !valid_host_port(host) {
        return Err(failure("Invalid alias prefix host"));
    }
    if value.contains('?') || value.contains('#') {
        return Err(failure("Alias prefix forbids query and fragment"));
    }
    if value.ends_with('/') {
        return Err(failure("Alias prefix must not end with a slash"));
    }
    if path.ends_with(".git") {
        return Err(failure("Alias prefix must not name a .git form"));
    }
    if path.contains("//") {
        return Err(failure("Alias prefix forbids empty path segments"));
    }
    let mut count = 0;
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        if !plain_segment(segment) || segment == "." || segment == ".." {
            return Err(failure("Alias prefix requires plain owner path segments"));
        }
        count += 1;
    }
    if count == 0 {
        return Err(failure(
            "Alias prefix requires explicit owner path segments",
        ));
    }
    Ok(())
}

/// Declared live primary feed validation: bare-origin pointer base, exact
/// bare-origin identity keys with opaque identity values, bounded timeout.
fn valid_primary_source(source: &PrimarySource) -> Result<(), Error> {
    if !clean_url_text(&source.pointer_base) {
        return Err(failure("Invalid primary source characters"));
    }
    let (scheme, rest) = split_origin(&source.pointer_base)
        .ok_or_else(|| failure("Primary source requires http(s)"))?;
    if scheme != "https" && scheme != "http" {
        return Err(failure("Primary source requires http(s)"));
    }
    if rest.contains('/') || rest.contains('@') || !valid_host_port(rest) {
        return Err(failure("Primary source must be a bare origin"));
    }
    if source.pointer_base.ends_with('/') {
        return Err(failure("Primary source must not end with a slash"));
    }
    if source.identities.is_empty() || source.identities.len() > 64 {
        return Err(failure(
            "Primary source needs an explicit nonempty identity map",
        ));
    }
    for (base, identity) in &source.identities {
        if !clean_url_text(base) {
            return Err(failure("Invalid primary identity base"));
        }
        let (scheme, rest) =
            split_origin(base).ok_or_else(|| failure("Identity base requires http(s)"))?;
        if scheme != "https" && scheme != "http" {
            return Err(failure("Identity base requires http(s)"));
        }
        if rest.contains('/') || rest.contains('@') || !valid_host_port(rest) {
            return Err(failure("Identity base must be a bare origin"));
        }
        if base.ends_with('/') {
            return Err(failure("Identity base must not end with a slash"));
        }
        if !plain_identity(identity) {
            return Err(failure("Identity must be an opaque validated ID"));
        }
    }
    if !(1..=60).contains(&source.timeout_secs) {
        return Err(failure("Primary source timeout must be 1..=60"));
    }
    Ok(())
}

/// Full repository URL (primary_url metadata, source_urls entries share the
/// shape): clean http(s) URL without userinfo/query/fragment.
fn valid_repo_url(value: &str) -> Result<(), Error> {
    if !clean_url_text(value) {
        return Err(failure("Invalid repository URL characters or encoding"));
    }
    let (_, rest) =
        split_origin(value).ok_or_else(|| failure("Repository URL requires http(s)"))?;
    let host = rest.split('/').next().unwrap_or_default();
    if host.contains('@') || !valid_host_port(host) {
        return Err(failure("Invalid repository URL host"));
    }
    if value.contains('?') || value.contains('#') {
        return Err(failure("Repository URL forbids query and fragment"));
    }
    Ok(())
}

fn valid_repo_path(value: &str) -> Result<(), Error> {
    if value.is_empty() || value.len() > MAX_PATH {
        return Err(failure("Invalid repository path length"));
    }
    let parts: Vec<_> = value.split('/').collect();
    if parts.len() < 2
        || parts.iter().any(|p| !plain_segment(p))
        || parts.iter().any(|p| *p == "." || *p == "..")
    {
        return Err(failure(
            "Repository path requires owner/repo segments of plain characters",
        ));
    }
    if value.ends_with(".git") {
        return Err(failure("Repository path must not carry a .git suffix"));
    }
    Ok(())
}

fn valid_scope(value: &str) -> Result<(), Error> {
    if value.is_empty() || value.len() > MAX_PATH {
        return Err(failure("Invalid store scope length"));
    }
    let parts: Vec<_> = value.split('/').collect();
    if parts.iter().any(|p| !plain_segment(p)) {
        return Err(failure(
            "Store scope requires plain owner[/repo...] segments",
        ));
    }
    Ok(())
}

/// Segment-boundary coverage: owner scope `acme` covers `acme` and every
/// path beneath it (`acme/widget`, `acme/sub/widget`), never `acme-evil/...`.
/// A multi-segment scope names one repository path and covers exactly that
/// path: `acme/widget` must not cover `acme/widget/sub`, which is a distinct
/// repository identity.
pub fn scope_covers(scope: &str, path: &str) -> bool {
    if scope.contains('/') {
        return path == scope;
    }
    path == scope || path.starts_with(&format!("{scope}/"))
}

fn valid_ref_name(value: &str, what: &str) -> Result<(), Error> {
    if value.is_empty() || value.len() > MAX_PATH {
        return Err(failure(format!("Invalid {what} length")));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b) || b == b'+')
        || value.contains("..")
        || value.starts_with('/')
        || value.ends_with('/')
        || value.ends_with(".lock")
        || value.bytes().any(|b| b" ~^:?*[]\\".contains(&b))
    {
        return Err(failure(format!("Invalid {what}")));
    }
    Ok(())
}

impl Request {
    pub fn validate(&self) -> Result<(), Error> {
        if self.schema != 1 {
            return Err(failure("Unsupported resolver request schema"));
        }
        valid_endpoint(&self.canonical_base, true)?;
        if self.canonical_base.ends_with('/') {
            return Err(failure("Canonical base must not end with a slash"));
        }
        if !(1..=300).contains(&self.timeout_secs) {
            return Err(failure("timeout_secs must be 1..=300"));
        }
        if self.repositories.is_empty() {
            return Err(failure("Request needs at least one repository"));
        }
        if self.repositories.len() > 1024 || self.stores.len() > 64 {
            return Err(failure("Request exceeds repository/store limits"));
        }
        for alias in &self.aliases {
            valid_alias_prefix(&alias.url_prefix)?;
            if !plain_segment(&alias.canonical_owner) {
                return Err(failure("Alias owner must be one plain segment"));
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        for repo in &self.repositories {
            if repo.id.is_empty() || repo.id.len() > 128 || !clean_url_text(&repo.id) {
                return Err(failure("Repository id must be short plain text"));
            }
            if !ids.insert(repo.id.as_str()) {
                return Err(failure("Duplicate repository id"));
            }
            valid_repo_path(&repo.path)?;
            match &repo.r#ref {
                RefKind::Pinned(sha) => {
                    if ![40, 64].contains(&sha.len())
                        || !sha
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    {
                        return Err(failure("Pinned ref requires a full lowercase commit hash"));
                    }
                }
                RefKind::Moving(gitref) => {
                    if !gitref.starts_with("refs/") {
                        return Err(failure("Moving ref must be a full refs/ name"));
                    }
                    valid_ref_name(gitref, "moving ref")?;
                }
            }
            if repo
                .primary
                .as_deref()
                .is_some_and(|primary| !plain_identity(primary))
            {
                return Err(failure("Primary must be an opaque validated identity"));
            }
            if let Some(url) = &repo.primary_url {
                valid_repo_url(url)?;
            }
            if repo.source_urls.is_empty() || repo.source_urls.len() > 64 {
                return Err(failure(
                    "Repository needs an explicit nonempty source_urls inventory",
                ));
            }
            let mut seen_urls = std::collections::BTreeSet::new();
            for url in &repo.source_urls {
                if !clean_url_text(url) || split_origin(url).is_none() {
                    return Err(failure("source_urls entries must be clean http(s) URLs"));
                }
                if url.contains('?') || url.contains('#') || url.contains('@') {
                    return Err(failure("source_urls forbids query, fragment and userinfo"));
                }
                if !seen_urls.insert(url.as_str()) {
                    return Err(failure("Duplicate source_urls entry"));
                }
                // Every declared fetch form must normalize back to this repo's
                // own canonical path; a cross-repo declaration fails closed.
                if normalize_url(self, url).as_deref() != Some(repo.path.as_str()) {
                    return Err(failure(
                        "source_urls entry does not normalize to its repository path",
                    ));
                }
            }
        }
        for store in &self.stores {
            store.validate()?;
        }
        if let Some(source) = &self.primary_source {
            valid_primary_source(source)?;
        }
        Ok(())
    }
}

impl Store {
    pub fn validate(&self) -> Result<(), Error> {
        if !plain_identity(&self.identity) {
            return Err(failure("Store identity must be an opaque validated ID"));
        }
        if self.scope.is_empty() || self.scope.len() > MAX_SCOPES {
            return Err(failure("Store needs an explicit nonempty scope allowlist"));
        }
        for scope in &self.scope {
            valid_scope(scope)?;
        }
        match self.kind {
            StoreKind::HttpForge => {
                valid_endpoint(&self.location, false)?;
                if self.location.ends_with('/') {
                    return Err(failure("Store location must not end with a slash"));
                }
                match self.provider.as_deref() {
                    // Only supported probe kinds route; anything else is a
                    // well-formed but skipped store (no hit, never guessed).
                    Some(provider) => {
                        let supported = !provider.is_empty()
                            && provider.len() <= 64
                            && provider.bytes().all(|b| {
                                b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b)
                            });
                        if !supported {
                            return Err(failure("HTTP store provider must name a probe kind"));
                        }
                    }
                    None => {
                        return Err(failure("HTTP store provider must name a probe kind"));
                    }
                }
                match &self.credential_env {
                    // Absent reference means an anonymous probe (public
                    // stores carry no token); a present reference must be
                    // a well-formed CFRG_RESOLVER_* name, never a value.
                    None => (),
                    Some(env) if valid_env_name(env) => (),
                    _ => {
                        return Err(failure(
                            "HTTP store needs a CFRG_RESOLVER_* credential reference",
                        ))
                    }
                }
                if self
                    .username
                    .as_deref()
                    .is_some_and(|user| !valid_username(user))
                {
                    return Err(failure(
                        "Store username must be alphanumeric-start plain ASCII",
                    ));
                }
                if self.trusted_single_user {
                    return Err(failure(
                        "trusted_single_user applies to filesystem stores only",
                    ));
                }
            }
            StoreKind::Filesystem => {
                if !self.location.starts_with('/')
                    || self.location.len() > MAX_URL
                    || self.location.bytes().any(|b| {
                        b.is_ascii_control() || b" \t\n\"'`<>{}|\\$;&()!*?[]#%".contains(&b)
                    })
                    || self.location.split('/').any(|s| s == "..")
                {
                    return Err(failure(
                        "Filesystem store needs an absolute escape-free root without URL-significant characters",
                    ));
                }
                if self.provider.is_some()
                    || self.credential_env.is_some()
                    || self.username.is_some()
                {
                    return Err(failure(
                        "Filesystem store takes no provider or credential fields",
                    ));
                }
                if !self.trusted_single_user {
                    return Err(failure(
                        "Filesystem store requires explicit trusted_single_user opt-in",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn covers(&self, path: &str) -> bool {
        self.scope.iter().any(|scope| scope_covers(scope, path))
    }
}

/// Canonical fetch URL for one repo on this store (the routed `via`).
pub fn store_repo_url(store: &Store, path: &str) -> String {
    match store.kind {
        StoreKind::HttpForge => format!("{}/{path}.git", store.location),
        StoreKind::Filesystem => format!("file://{}/{path}.git", store.location),
    }
}

/// Strict `scheme://host:port` scope for one origin, filling the default
/// port when absent and lowercasing the host (DNS is case-insensitive) so
/// both credential sides compare precisely the same normalized authority.
/// Returns `None` when malformed.
pub fn normalize_origin(location: &str) -> Option<String> {
    let (scheme, rest) = split_origin(location)?;
    if scheme != "https" && scheme != "http" {
        return None;
    }
    if rest.contains('/') || rest.contains('?') || rest.contains('#') || rest.contains('@') {
        return None;
    }
    let (host, port) = match rest.split_once(':') {
        Some((host, port)) => (host, parse_port(port)?.to_string()),
        None => (
            rest,
            if scheme == "https" {
                "443".to_owned()
            } else {
                "80".to_owned()
            },
        ),
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
    {
        return None;
    }
    Some(format!("{scheme}://{}:{port}", host.to_ascii_lowercase()))
}

/// Canonical pointer URL for one repo (fallback; always the canonical base).
pub fn canonical_url(base: &str, path: &str) -> String {
    format!("{base}/{path}")
}

/// Normalize an arbitrary fetch URL to its canonical repo path using the
/// canonical base first, then explicit aliases, all with segment boundaries.
/// Returns `None` when the URL is not a safely routable owned URL.
pub fn normalize_url(request: &Request, url: &str) -> Option<String> {
    if !clean_url_text(url) {
        return None;
    }
    if let Some(rest) = strip_base(&request.canonical_base, url) {
        return repo_identity(rest);
    }
    for alias in &request.aliases {
        if let Some(rest) = strip_base(&alias.url_prefix, url) {
            let combined = if rest.is_empty() {
                alias.canonical_owner.clone()
            } else {
                format!("{}/{rest}", alias.canonical_owner)
            };
            // Same identity rules as the canonical branch: one trailing
            // `.git` (or slash) denotes the same repository.
            if let Some(identity) = repo_identity(&combined) {
                return Some(identity);
            }
        }
    }
    None
}

/// Strip a prefix only at a segment boundary (end, `/`, or `.git` form).
fn strip_base<'a>(prefix: &'a str, url: &'a str) -> Option<&'a str> {
    let rest = url.strip_prefix(prefix)?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix('/')
}

/// Canonical identity from the path remainder: strip one `.git` suffix and
/// optional trailing slash, then enforce repo-path shape.
fn repo_identity(rest: &str) -> Option<String> {
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    if rest.ends_with(".git") || rest.is_empty() {
        return None;
    }
    if valid_repo_path(rest).is_ok() {
        Some(rest.to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_matching_respects_segment_boundaries() {
        assert!(scope_covers("acme", "acme/widget"));
        assert!(scope_covers("acme", "acme/sub/widget"));
        assert!(scope_covers("acme/widget", "acme/widget"));
        assert!(!scope_covers("acme", "acme-evil/widget"));
        assert!(!scope_covers("acme/widget", "acme/widget-evil"));
        assert!(!scope_covers("acme/widget", "acme/widget/sub"));
    }

    #[test]
    fn alias_prefix_matching_rejects_org_prefix_attacks() {
        let request = Request {
            schema: 1,
            canonical_base: "https://pointer.example".into(),
            aliases: vec![Alias {
                url_prefix: "https://github.com/acme".into(),
                canonical_owner: "acme".into(),
            }],
            repositories: vec![],
            stores: vec![],
            timeout_secs: 30,
            primary_source: None,
        };
        assert_eq!(
            normalize_url(&request, "https://github.com/acme/widget"),
            Some("acme/widget".into())
        );
        assert_eq!(
            normalize_url(&request, "https://github.com/acme/widget.git"),
            Some("acme/widget".into())
        );
        assert_eq!(
            normalize_url(&request, "https://github.com/acme-evil/x"),
            None
        );
        assert_eq!(
            normalize_url(&request, "https://pointer.example/acme/widget-evil"),
            Some("acme/widget-evil".into())
        );
        assert_eq!(
            normalize_url(&request, "https://pointer.example/acme-evil/x"),
            Some("acme-evil/x".into())
        );
        assert_eq!(
            normalize_url(&request, "https://evil.example/acme/widget"),
            None
        );
    }

    #[test]
    fn endpoint_validation_rejects_credentials_queries_and_cleartext_base() {
        assert!(valid_endpoint("https://host.example:8443", true).is_ok());
        assert!(valid_endpoint("https://host.example/", true).is_ok());
        assert!(valid_endpoint("http://host.example:3001", true).is_err());
        assert!(valid_endpoint("http://host.example:3001", false).is_ok());
        // Origins only: any path prefix is rejected so a location IS the
        // credential origin (no separate derivation).
        for bad in [
            "https://user@host.example",
            "https://host.example/prefix",
            "https://host.example/x?y=1",
            "https://host.example/x#frag",
            "https://host.example/../x",
            "https://host.example/x y",
            "https://host.example/x%zz",
            "https://host.example:999999",
            "https://host.example:0",
            "https://host.example:00000",
            "https://host.example:007",
            "https://host.example:65536",
        ] {
            assert!(valid_endpoint(bad, false).is_err(), "{bad}");
        }
        // Full repo URLs validate separately (primary_url metadata shape).
        assert!(valid_repo_url("https://github.com/acme/widget").is_ok());
        assert!(valid_repo_url("https://github.com/acme/widget?x=1").is_err());
    }

    #[test]
    fn malformed_percent_encodings_fail_without_panic() {
        // Regression: `%` and `%A` once indexed past the end.
        for bad in ["%", "%A", "a%", "a%2", "%zz", "%2", "x%4G", "%%", "%2F%"] {
            assert!(bad_percent_encoding(bad), "{bad:?}");
            assert!(valid_endpoint(&format!("https://host.example/{bad}"), false).is_err());
        }
        for good in ["%2F", "%41", "a%20b", "plain"] {
            assert!(!bad_percent_encoding(good), "{good:?}");
        }
    }

    #[test]
    fn ports_require_nonzero_u16_canonical_spelling() {
        assert_eq!(
            normalize_origin("https://host.example"),
            Some("https://host.example:443".into())
        );
        assert_eq!(
            normalize_origin("http://host.example:3001"),
            Some("http://host.example:3001".into())
        );
        assert_eq!(
            normalize_origin("https://HOST.example:443"),
            Some("https://host.example:443".into())
        );
        assert_eq!(
            normalize_origin("https://host.example:65535"),
            Some("https://host.example:65535".into())
        );
        for bad in [
            "https://host.example:0",
            "https://host.example:00",
            "https://host.example:0443",
            "https://host.example:65536",
            "https://host.example:99999",
            "https://host.example:",
            "https://host.example:4x3",
        ] {
            assert_eq!(normalize_origin(bad), None, "{bad}");
        }
    }

    #[test]
    fn alias_prefixes_require_explicit_plain_owner_paths() {
        // Contract example and owned personal mapping both validate.
        for good in [
            "https://github.com/acme",
            "https://github.com/julian-corbet",
            "http://forge.internal:3001/acme/sub",
            "https://host.example:8443/a/b/c",
        ] {
            assert!(valid_alias_prefix(good).is_ok(), "{good}");
        }
        for bad in [
            "https://github.com",
            "https://github.com/",
            "https://github.com/acme/",
            "https://github.com/acme/../evil",
            "https://github.com/acme//evil",
            "https://github.com/./acme",
            "https://user@github.com/acme",
            "https://github.com/acme?x=1",
            "https://github.com/acme#frag",
            "https://github.com/acme/widget.git",
            "https://github.com/acme%2f evil",
            "https://github.com/acme%zz",
            "git@github.com:acme",
            "https://github.com:0/acme",
        ] {
            assert!(valid_alias_prefix(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn usernames_reject_shell_and_config_breakout() {
        for good in ["ci", "Forgejo-ci.user_1", "a"] {
            assert!(valid_username(good), "{good}");
        }
        for bad in [
            "",
            "-ci",
            ".ci",
            "#ci",
            "ci%x",
            "ci user",
            "ci;id",
            "ci$(id)",
            "ci`id`",
            "ci'quote",
            "ci\"quote",
            "ci\\x",
            "user@host",
            &"u".repeat(65),
        ] {
            assert!(!valid_username(bad), "{bad:?}");
        }
    }

    #[test]
    fn primary_source_validation() {
        use std::collections::BTreeMap;
        let good = PrimarySource {
            pointer_base: "https://pointer.example".into(),
            identities: BTreeMap::from([("https://forge.example:3001".into(), "forgejo".into())]),
            timeout_secs: 10,
        };
        assert!(valid_primary_source(&good).is_ok());
        for mutate in [
            |s: &mut PrimarySource| s.pointer_base = "http://pointer.example/prefix".into(),
            |s: &mut PrimarySource| s.pointer_base = "https://pointer.example/".into(),
            |s: &mut PrimarySource| s.pointer_base = "https://user@pointer.example".into(),
            |s: &mut PrimarySource| s.identities.clear(),
            |s: &mut PrimarySource| {
                s.identities
                    .insert("https://other.example/x".into(), "hub".into());
            },
            |s: &mut PrimarySource| {
                s.identities
                    .insert("https://forge.example:3001".into(), "Bad Name".into());
            },
            |s: &mut PrimarySource| s.timeout_secs = 0,
            |s: &mut PrimarySource| s.timeout_secs = 61,
        ] {
            let mut bad = good.clone();
            mutate(&mut bad);
            assert!(valid_primary_source(&bad).is_err());
        }
    }
}
