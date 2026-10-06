//! Read-only forge-API evidence collection for the cqlt quality library.
//!
//! `cfrg collect` snapshots visible organizations and repositories through
//! public forge APIs without writes. Offline evaluation of saved evidence
//! stays with cqlt (and `ccid quality check`); this module only gathers facts.
use crate::{model::Forge, process::Runner, Environment, Result};
use clap::Args;
use cqlt::{Document, Organization, Repository, Snapshot, Visibility};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn failure(message: impl Into<String>) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::other(message.into()).into()
}

/// How one forge exposes organizations and repositories to evidence
/// collection. Implemented once per adapter (`cghb`, `cfgj`); the collector
/// stays forge-independent and receives the dialect from the CLI.
pub trait EvidenceSource {
    /// Source identity label recorded on snapshots (for example `Github`).
    /// The `source` field keeps the long-standing `{label}:{api-root}`
    /// shape; only the label moved here from a `Debug` format.
    fn source_label(&self) -> &'static str;
    /// Default API root, or `None` when `--api-url` is always required.
    fn default_api_base(&self) -> Option<&'static str>;
    /// Whether an existing `gh` login may supply the credential.
    fn prefers_gh_login(&self) -> bool;
    /// Pagination size parameter: `per_page` or `limit`.
    fn page_param(&self) -> &'static str;
    /// Membership-organization identity field.
    fn scope_login_field(&self) -> &'static str;
    /// Repository inventory path for one organization.
    fn repos_path(&self, login: &str) -> String;
    /// Organization profile repository name.
    fn profile_repo(&self) -> &'static str;
    /// Repository homepage field.
    fn repo_homepage_field(&self) -> &'static str;
    /// Organization display-name field.
    fn org_name_field(&self) -> &'static str;
    /// Organization website field.
    fn org_website_field(&self) -> &'static str;
    /// Default-branch commit identity field.
    fn branch_commit_field(&self) -> &'static str;
    /// Repository topics, embedded or via a dedicated endpoint.
    fn repo_topics(
        &self,
        transport: &dyn Transport,
        path: &str,
        row: &Value,
    ) -> Result<Vec<String>>;
    /// Repository README, natively or listed from the contents.
    fn repo_readme(
        &self,
        transport: &dyn Transport,
        path: &str,
        revision: &str,
        root: &[Value],
    ) -> Result<Document>;
    /// Organization profile document for a profile repository.
    fn org_profile(
        &self,
        transport: &dyn Transport,
        path: &str,
        revision: &str,
        root: Vec<Value>,
    ) -> Result<Document>;
}

/// Collect visible organizations and repositories using read-only forge APIs.
#[derive(Args)]
pub struct Options {
    #[arg(long, value_enum)]
    pub forge: Forge,
    /// API root. Defaults to GitHub; required for Forgejo. HTTPS only.
    #[arg(long)]
    pub api_url: Option<String>,
    /// Omit to discover every organization accessible through membership.
    #[arg(long = "org")]
    pub organizations: Vec<String>,
    #[arg(long)]
    pub output: PathBuf,
    #[arg(long, default_value_t = 900, value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout: u64,
}

/// Collect forge evidence into `options.output`. Returns 0 when the snapshot
/// is complete, 2 when some organizations failed and were recorded as errors.
pub fn run(options: Options, source: &'static dyn EvidenceSource) -> Result<u8> {
    let Options {
        // The dialect, resolved by the CLI from this flag, carries every
        // forge-specific rule including the snapshot source label.
        forge: _,
        api_url,
        organizations,
        output,
        timeout,
    } = options;
    let base = match api_url {
        Some(url) => url.trim_end_matches('/').to_owned(),
        None => source
            .default_api_base()
            .ok_or_else(|| failure("This forge requires --api-url https://forge.example/api/v1"))?
            .to_owned(),
    };
    validate_base(&base)?;
    for org in &organizations {
        if !cqlt::valid_login(org) {
            return Err(failure("Invalid organization name"));
        }
    }
    let mut environment: Environment = std::env::vars_os().collect();
    let token = if let Ok(token) = std::env::var("CFRG_TOKEN") {
        token
    } else if source.prefers_gh_login() && Some(base.as_str()) == source.default_api_base() {
        let runner = Runner::new(
            std::env::current_dir()?,
            environment.clone(),
            Duration::from_secs(30),
        )?;
        runner.run(
            &[
                "gh".into(),
                "auth".into(),
                "token".into(),
                "--hostname".into(),
                "github.com".into(),
            ],
            true,
        )?
    } else {
        return Err(failure("Set CFRG_TOKEN for the selected forge instance"));
    };
    if token.trim().is_empty() || token.contains(['\r', '\n']) {
        return Err(failure("Invalid CFRG_TOKEN"));
    }
    environment.insert("CFRG_COLLECT_TOKEN".into(), token.into());
    let runner = Runner::new(
        std::env::current_dir()?,
        environment,
        Duration::from_secs(timeout),
    )?;
    let client = Client {
        runner,
        base: base.clone(),
    };
    let origin = format!("{}:{base}", source.source_label());
    let snapshot = collect(&client, source, organizations, origin)?;
    let complete = snapshot.complete;
    write_snapshot(&output, &snapshot)?;
    eprintln!(
        "Saved {} organizations to {} (complete={complete})",
        snapshot.organizations.len(),
        output.display()
    );
    Ok(if complete { 0 } else { 2 })
}

fn validate_base(base: &str) -> Result<()> {
    let host = base
        .strip_prefix("https://")
        .ok_or_else(|| failure("API URL must use HTTPS"))?;
    if host.is_empty()
        || host.starts_with('/')
        || base.contains(['@', '?', '#', '\\'])
        || base.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(failure(
            "API URL must identify an HTTPS instance without credentials, query or fragment",
        ));
    }
    Ok(())
}

pub fn component(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Bounded evidence transport: plain HTTPS GET with credential expansion
/// inside curl. The core `Client` speaks to any API root; adapters resolve
/// the exact paths per forge.
pub trait Transport {
    fn get(&self, path: &str) -> Result<Option<Value>>;
}

struct Client {
    runner: Runner,
    base: String,
}
impl Transport for Client {
    fn get(&self, path: &str) -> Result<Option<Value>> {
        let body = tempfile::NamedTempFile::new()?;
        // curl expands the inherited variable internally; tokens never appear in
        // argv, logs, snapshots or files. Redirects are deliberately not followed.
        let args = vec![
            "curl".into(),
            "--disable".into(),
            "--silent".into(),
            "--show-error".into(),
            "--globoff".into(),
            "--connect-timeout".into(),
            "10".into(),
            "--max-time".into(),
            "30".into(),
            "--max-filesize".into(),
            "16777216".into(),
            "--proto".into(),
            "=https".into(),
            "--variable".into(),
            "%CFRG_COLLECT_TOKEN".into(),
            "--expand-header".into(),
            "Authorization: token {{CFRG_COLLECT_TOKEN}}".into(),
            "--header".into(),
            "Accept: application/json".into(),
            "--output".into(),
            body.path().to_string_lossy().into_owned(),
            "--write-out".into(),
            "%{http_code}".into(),
            "--url".into(),
            format!("{}/{}", self.base, path),
        ];
        let status = self.runner.run(&args, true)?;
        match status.as_str() {
            "200" => Ok(Some(serde_json::from_slice(&fs::read(body.path())?)?)),
            "404" => Ok(None),
            _ => Err(failure(format!("Forge GET {path} returned HTTP {status}"))),
        }
    }
}

pub fn required(api: &dyn Transport, path: &str) -> Result<Value> {
    api.get(path)?
        .ok_or_else(|| failure(format!("Required forge resource unavailable: {path}")))
}

pub fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| failure(format!("Missing string field {key}")))
}
pub fn optional(v: &Value, key: &str) -> Result<Option<String>> {
    match v.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(failure(format!("Missing nullable string field {key}"))),
    }
}
pub fn boolean(v: &Value, key: &str) -> Result<bool> {
    v.get(key)
        .and_then(Value::as_bool)
        .ok_or_else(|| failure(format!("Missing boolean field {key}")))
}

/// Paginated inventory: requests until an EMPTY page, since instances may
/// cap page size below the requested one.
pub fn pages(api: &dyn Transport, source: &dyn EvidenceSource, path: &str) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    let mut identities = std::collections::BTreeSet::new();
    // Request until an EMPTY page, not a short page: instances may cap page size.
    for page in 1..=10000 {
        let size = source.page_param();
        let separator = if path.contains('?') { '&' } else { '?' };
        let data = required(api, &format!("{path}{separator}{size}=100&page={page}"))?;
        let rows = data
            .as_array()
            .ok_or_else(|| failure("Expected a paginated array"))?;
        if rows.is_empty() {
            return Ok(all);
        }
        for row in rows {
            let id = row
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| failure("Missing paginated identity"))?;
            if !identities.insert(id) {
                return Err(failure(
                    "Pagination repeated an identity; recollect a stable inventory",
                ));
            }
            all.push(row.clone());
        }
    }
    Err(failure("Pagination limit exceeded"))
}

fn collect(
    api: &dyn Transport,
    source: &dyn EvidenceSource,
    mut scope: Vec<String>,
    origin: String,
) -> Result<Snapshot> {
    if scope.is_empty() {
        scope = pages(api, source, "user/orgs")?
            .iter()
            .map(|o| text(o, source.scope_login_field()).map(str::to_owned))
            .collect::<Result<_>>()?;
    }
    scope.sort();
    scope.dedup();
    if scope.is_empty() {
        return Err(failure(
            "No organizations discovered; refusing an empty passing audit",
        ));
    }
    let mut snapshot = Snapshot {
        schema: 1,
        source: origin,
        collected_at: format!(
            "unix:{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
        ),
        scope,
        complete: true,
        errors: Vec::new(),
        organizations: Vec::new(),
    };
    for login in &snapshot.scope {
        if !cqlt::valid_login(login) {
            return Err(failure("Invalid organization identity from forge"));
        }
        match organization(api, source, login) {
            Ok(org) => {
                if matches!(org.profile, Document::Unknown { .. })
                    || org.repositories.iter().any(|r| {
                        matches!(r.readme, Document::Unknown { .. })
                            || matches!(r.license, Document::Unknown { .. })
                    })
                {
                    snapshot.complete = false;
                }
                snapshot.organizations.push(org);
            }
            Err(error) => {
                snapshot.complete = false;
                snapshot.errors.push(format!("{login}: {error}"));
            }
        }
    }
    Ok(snapshot)
}

fn organization(
    api: &dyn Transport,
    source: &dyn EvidenceSource,
    login: &str,
) -> Result<Organization> {
    let meta = required(api, &format!("orgs/{login}"))?;
    let rows = pages(api, source, &source.repos_path(login))?;
    let count = rows.len();
    let mut repositories = Vec::new();
    let profile_repo = source.profile_repo();
    let mut profile = Document::Missing;
    for row in rows {
        let mut repo = Repository {
            name: text(&row, "name")?.into(),
            full_name: text(&row, "full_name")?.into(),
            description: optional(&row, "description")?,
            homepage: optional(&row, source.repo_homepage_field())?,
            visibility: if boolean(&row, "private")? {
                Visibility::Private
            } else if row.get("visibility").and_then(Value::as_str) == Some("internal") {
                Visibility::Internal
            } else {
                Visibility::Public
            },
            fork: boolean(&row, "fork")?,
            archived: boolean(&row, "archived")?,
            revision: None,
            topics: Vec::new(),
            readme: Document::Unknown {
                reason: "Not collected".into(),
            },
            license: Document::Unknown {
                reason: "Not collected".into(),
            },
        };
        if repo.full_name != format!("{login}/{}", repo.name) {
            return Err(failure("Repository belongs to a different organization"));
        }
        let path = format!("repos/{}/{}", component(login), component(&repo.name));
        repo.topics = source.repo_topics(api, &path, &row)?;
        let branch = optional(&row, "default_branch")?.unwrap_or_default();
        if branch.is_empty() || row.get("empty") == Some(&Value::Bool(true)) {
            repo.readme = Document::Missing;
            repo.license = Document::Missing;
        } else {
            match repository_documents(api, source, &path, &branch, repo.name == profile_repo) {
                Ok((revision, readme, license, org_profile)) => {
                    repo.revision = Some(revision);
                    repo.readme = readme;
                    repo.license = license;
                    if repo.name == profile_repo {
                        profile = org_profile;
                    }
                }
                Err(error) => {
                    let unknown = Document::Unknown {
                        reason: error.to_string(),
                    };
                    repo.readme = unknown.clone();
                    repo.license = unknown.clone();
                    if repo.name == profile_repo {
                        profile = unknown;
                    }
                }
            }
        }
        repositories.push(repo);
    }
    let org = Organization {
        login: login.into(),
        name: optional(&meta, source.org_name_field())?,
        description: optional(&meta, "description")?,
        website: optional(&meta, source.org_website_field())?,
        profile,
        repository_count: count,
        repositories,
    };
    Ok(org)
}

pub fn listed_document(rows: &[Value], prefix: &str, names: &[&str]) -> Result<Document> {
    let mut candidates: Vec<_> = rows
        .iter()
        .filter(|e| {
            e.get("type").and_then(Value::as_str) == Some("file")
                && e.get("name").and_then(Value::as_str).is_some_and(|name| {
                    names.iter().any(|stem| {
                        let name = name.to_ascii_uppercase();
                        name == *stem
                            || name
                                .strip_prefix(stem)
                                .is_some_and(|suffix| suffix.starts_with(['.', '-', '_']))
                    })
                })
        })
        .collect();
    candidates.sort_by_key(|e| e.get("name").and_then(Value::as_str));
    for entry in candidates {
        let bytes = entry
            .get("size")
            .and_then(Value::as_u64)
            .ok_or_else(|| failure("Missing document size"))?;
        if bytes > 0 {
            return Ok(Document::Present {
                path: format!("{prefix}{}", text(entry, "name")?),
                bytes,
            });
        }
    }
    Ok(Document::Missing)
}

pub fn directory(api: &dyn Transport, path: &str) -> Result<Vec<Value>> {
    required(api, path)?
        .as_array()
        .cloned()
        .ok_or_else(|| failure("Expected contents directory"))
}

fn repository_documents(
    api: &dyn Transport,
    source: &dyn EvidenceSource,
    path: &str,
    branch: &str,
    is_profile: bool,
) -> Result<(String, Document, Document, Document)> {
    let branch = required(api, &format!("{path}/branches/{}", component(branch)))?;
    let commit = branch
        .get("commit")
        .ok_or_else(|| failure("Missing branch commit"))?;
    let revision = text(commit, source.branch_commit_field())?.to_owned();
    if revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(failure("Invalid branch revision"));
    }
    let root = directory(api, &format!("{path}/contents?ref={revision}"))?;
    let license = listed_document(&root, "", &["LICENSE", "LICENCE", "COPYING"])?;
    let mut readme = source.repo_readme(api, path, &revision, &root)?;
    let mut profile = Document::Missing;
    if is_profile {
        profile = source.org_profile(api, path, &revision, root)?;
        if matches!(readme, Document::Missing) {
            readme = profile.clone();
        }
    }
    Ok((revision, readme, license, profile))
}

fn write_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut file, snapshot)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests;
