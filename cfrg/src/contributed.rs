//! Contributed projects (operating model R10, R11).
//!
//! A contributed project is someone else's project that we contribute to. Its releases and issues are not mirrored or
//! relayed, our compute does not run its CI, and nothing of ours is put into its repository. The class is declared in
//! the placement document (`contributed`, see `docs/contributed.md`); this module is the one place that reads it, for
//! the mirror reconciler and for the router.
//!
//! Locations are matched as `host/owner/repo` in lowercase, in any spelling git accepts (https, ssh, scp-like, with
//! or without `.git`). ccid reads the same declaration with the same rules (its `docs/contributed.md`).
use crate::{
    failure,
    native::{ContributedProject, Placement, Provider},
    Result,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

/// The contributed projects of one placement, indexed by every location they have.
#[derive(Debug, Clone)]
pub struct Contributed {
    projects: Vec<ContributedProject>,
    /// Lowercase `host/path` of every location → index of its project.
    keys: BTreeMap<String, usize>,
    /// Every spelling of our forge: a location on one of them is also known under all the others.
    forge_hosts: Vec<String>,
    /// The host of the placement's source forge, the only one whose repositories cfrg can switch settings on.
    source_host: String,
}

impl Contributed {
    /// The index of a placement's `contributed` list. `extra_forge_hosts` are more spellings of the source forge that
    /// the caller knows (a public alias): a location on our forge is then known under each of them.
    ///
    /// Refused: a name that is not plain and lowercase, names out of order, a location that is not the canonical
    /// `https://host/owner/repo`, a location in two projects, and a project that has mirror destinations.
    pub fn from_placement(placement: &Placement, extra_forge_hosts: &[&str]) -> Result<Self> {
        let source_host = host_of(&placement.source.origin)
            .ok_or_else(|| failure("The placement source is not an https origin"))?;
        // Our forge is the placement's source. (`default` names the default PRIMARY, which a primary switch can point
        // at another forge: it is no spelling of ours.)
        let mut forge_hosts = vec![source_host.clone()];
        for host in extra_forge_hosts.iter().map(|h| h.to_ascii_lowercase()) {
            if !forge_hosts.contains(&host) {
                forge_hosts.push(host);
            }
        }
        let mut keys: BTreeMap<String, usize> = BTreeMap::new();
        let mut previous: Option<&str> = None;
        for (index, project) in placement.contributed.iter().enumerate() {
            validate_name(&project.name)?;
            if previous.is_some_and(|before| before >= project.name.as_str()) {
                return Err(failure(format!(
                    "Contributed projects are in strictly ascending order: {} follows {}",
                    project.name,
                    previous.unwrap_or_default()
                )));
            }
            previous = Some(&project.name);
            let mut seen = BTreeSet::new();
            for location in std::iter::once(&project.upstream).chain(&project.forks) {
                if !seen.insert(location.as_str()) {
                    return Err(failure(format!(
                        "Contributed project {}: {location} is listed twice",
                        project.name
                    )));
                }
                let key = canonical_key(location).map_err(|error| {
                    failure(format!("Contributed project {}: {error}", project.name))
                })?;
                let host = key.split('/').next().unwrap_or_default().to_owned();
                let spellings: Vec<String> = if forge_hosts.contains(&host) {
                    forge_hosts
                        .iter()
                        .map(|h| format!("{h}{}", &key[host.len()..]))
                        .collect()
                } else {
                    vec![key]
                };
                for spelling in spellings {
                    if let Some(other) = keys.insert(spelling.clone(), index) {
                        if other != index {
                            return Err(failure(format!(
                                "Contributed projects {} and {} share {spelling}",
                                placement.contributed[other].name, project.name
                            )));
                        }
                    }
                }
            }
        }
        let this = Self {
            projects: placement.contributed.clone(),
            keys,
            forge_hosts,
            source_host,
        };
        // R2 names an explicit exclusion as the only way a repository is left out of the mirrors; R11 makes a
        // contributed project one. A declared mirror of it contradicts the declaration, in both directions: the source
        // repository of a contributed project has no destination, and no destination is a contributed project.
        // (A destination marked `absent` is a mirror being retired, which is how an existing one goes.)
        for repository in &placement.repositories {
            let live = repository.destinations.iter().filter(|d| !d.absent);
            if live.clone().next().is_some() {
                if let Some(project) = this.classify(&this.source_host, &repository.path) {
                    return Err(failure(format!(
                        "{} is the contributed project {}: it has no mirror destination (operating model R11)",
                        repository.path, project.name
                    )));
                }
            }
            for destination in live {
                let host = match destination.provider {
                    Provider::Github => "github.com".to_owned(),
                    Provider::Bitbucket => "bitbucket.org".to_owned(),
                    Provider::Gitlab => host_of(&destination.endpoint.origin)
                        .ok_or_else(|| failure("A GitLab destination is not an https origin"))?,
                };
                let path = repository.destination_path(destination);
                if let Some(project) = this.classify(&host, &path) {
                    return Err(failure(format!(
                        "{} would be mirrored into {host}/{path}, the contributed project {} (operating model R11)",
                        repository.path, project.name
                    )));
                }
            }
        }
        Ok(this)
    }

    /// Read a placement file (the whole document or its native view) and index its contributed projects.
    pub fn load(path: &Path, extra_forge_hosts: &[&str]) -> Result<Self> {
        let placement = Placement::from_document(&fs::read(path)?)?;
        placement.validate()?;
        Self::from_placement(&placement, extra_forge_hosts)
    }

    pub fn projects(&self) -> &[ContributedProject] {
        &self.projects
    }

    /// Every spelling of the source forge this index knows.
    pub fn forge_hosts(&self) -> &[String] {
        &self.forge_hosts
    }

    pub fn is_empty(&self) -> bool {
        self.projects.is_empty()
    }

    /// The project a repository URL belongs to, in any spelling git accepts.
    pub fn classify_url(&self, url: &str) -> Option<&ContributedProject> {
        self.keys
            .get(&key_of(url)?)
            .map(|index| &self.projects[*index])
    }

    /// The project of `owner/repo` on the forge reached as `forge_host` (a bare host name or an https origin).
    ///
    /// A provider's API origin names its web host: `api.github.com` is `github.com`, `api.bitbucket.org` is
    /// `bitbucket.org` (a destination of the placement is reached through the API origin).
    pub fn classify(&self, forge_host: &str, path: &str) -> Option<&ContributedProject> {
        let host = host_of(forge_host).or_else(|| Some(forge_host.to_ascii_lowercase()))?;
        let host = match host.as_str() {
            "api.github.com" => "github.com".to_owned(),
            "api.bitbucket.org" => "bitbucket.org".to_owned(),
            _ => host,
        };
        self.classify_url(&format!("https://{host}/{}", path.trim_matches('/')))
    }

    /// `Err` when `owner/repo` on `forge_host` is a contributed project, naming `operation`. The router calls this
    /// before any write for a repository (RD12), on the primary and on every copy.
    pub fn refuse(&self, operation: &str, forge_host: &str, path: &str) -> Result<()> {
        match self.classify(forge_host, path) {
            Some(project) => Err(refusal(operation, project)),
            None => Ok(()),
        }
    }

    /// [`Contributed::refuse`] for a repository URL.
    pub fn refuse_url(&self, operation: &str, url: &str) -> Result<()> {
        match self.classify_url(url) {
            Some(project) => Err(refusal(operation, project)),
            None => Ok(()),
        }
    }

    /// `owner/repo` of every contributed repository on the placement's source forge, sorted: the repositories whose
    /// Forgejo Actions the reconciler keeps off.
    pub fn source_paths(&self) -> Vec<String> {
        let prefix = format!("{}/", self.source_host);
        let mut paths = BTreeSet::new();
        for project in &self.projects {
            for location in std::iter::once(&project.upstream).chain(&project.forks) {
                if let Some(key) = key_of(location) {
                    if let Some(path) = key.strip_prefix(&prefix) {
                        // Forgejo repositories are owner/repo; a longer path is another forge's nesting.
                        if path.split('/').count() == 2 {
                            paths.insert(path.to_owned());
                        }
                    }
                }
            }
        }
        paths.into_iter().collect()
    }
}

fn refusal(
    operation: &str,
    project: &ContributedProject,
) -> Box<dyn std::error::Error + Send + Sync> {
    failure(format!(
        "{operation} refused: {} is a contributed project (operating model R11): its releases and issues are not \
         mirrored or relayed, and its CI does not run on our compute",
        project.name
    ))
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        return Err(failure(format!(
            "Contributed project name {name:?} is not a plain lowercase name"
        )));
    }
    Ok(())
}

/// The lowercase host of an https origin or of a bare host name.
fn host_of(origin: &str) -> Option<String> {
    let rest = origin
        .trim()
        .strip_prefix("https://")
        .unwrap_or(origin.trim());
    let host = rest.split('/').next()?.rsplit('@').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b)))
    .then(|| host.to_ascii_lowercase())
}

/// `host/path` in lowercase, for matching: scheme, user information, port, query, a trailing slash and one `.git`
/// are dropped; scp-like (`git@host:owner/repo`) is understood. `None` when `url` names no `host/owner/repo`.
pub fn key_of(url: &str) -> Option<String> {
    let url = url.trim();
    let url = url.strip_prefix("git+").unwrap_or(url);
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !["https", "http", "ssh", "git"].contains(&scheme.to_ascii_lowercase().as_str()) {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host_port = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (host_port.split(':').next()?, path)
    } else {
        let (left, path) = url.split_once(':')?;
        if left.contains('/') {
            return None;
        }
        (left.rsplit_once('@').map_or(left, |(_, host)| host), path)
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b))
    {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() < 2
        || segments
            .iter()
            .any(|s| s.is_empty() || *s == "." || *s == "..")
    {
        return None;
    }
    Some(format!("{host}/{path}").to_ascii_lowercase())
}

/// The key of a location as the placement writes it: exactly `https://host/owner/repo` (lowercase host, no user
/// information, port, `.git`, trailing slash, query or fragment).
fn canonical_key(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| failure(format!("{url}: a location is an https URL")))?;
    let valid = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    let (host, path) = rest
        .split_once('/')
        .ok_or_else(|| failure(format!("{url}: a location names host/owner/repo")))?;
    let segments: Vec<&str> = path.split('/').collect();
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-.".contains(&b))
        || segments.len() < 2
        || !segments.iter().all(|s| valid(s))
        || path.ends_with(".git")
    {
        return Err(failure(format!(
            "{url}: not a canonical location (lowercase host, host/owner/repo, no port, user, .git or trailing slash)"
        )));
    }
    Ok(format!("{host}/{path}").to_ascii_lowercase())
}

#[cfg(test)]
mod tests;
