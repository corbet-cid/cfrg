//! Typed write capabilities for the relay, through GitHub App authentication.
//!
//! The GitHub App `corbet-forge` holds Metadata read, Contents write, Issues
//! write and Pull requests write and nothing else (Bridge design, section 8).
//! [`octocrab`] mints and refreshes the installation token (JWT, then
//! `POST /app/installations/{id}/access_tokens`); this module only exposes the
//! operations the relay needs. There is deliberately no public method that
//! takes a route, a verb or a free-form body.
//!
//! The API is async (octocrab is); a sync caller wraps it in a runtime.

use std::fmt;

use octocrab::models::{AppId, InstallationId};
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};

/// Prefix of every branch the relay creates on a GitHub copy.
pub const RELAY_BRANCH_PREFIX: &str = "cforge/";

/// Errors of the relay driver.
#[derive(Debug)]
pub enum Error {
    /// An argument failed validation before any request was made.
    Invalid(String),
    /// The request failed. The text never contains a credential.
    Api(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(m) => write!(f, "invalid argument: {m}"),
            Error::Api(m) => write!(f, "github api: {m}"),
        }
    }
}
impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn api(e: octocrab::Error) -> Error {
    // Only the variant's own message: never the request headers.
    Error::Api(match &e {
        octocrab::Error::GitHub { source, .. } => {
            format!("{} ({})", source.message, source.status_code)
        }
        other => other.to_string(),
    })
}

/// A string that never prints itself.
#[derive(Clone)]
pub struct Redacted(String);
impl Redacted {
    pub fn new(value: impl Into<String>) -> Self {
        Redacted(value.into())
    }
}
impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Redacted(..)")
    }
}

/// App identity: app id, installation id and the PEM private key.
#[derive(Clone, Debug)]
pub struct AppCredentials {
    pub app_id: u64,
    pub installation_id: u64,
    pub private_key_pem: Redacted,
}

/// `owner/name`, validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    owner: String,
    name: String,
}

impl Repo {
    pub fn parse(full: &str) -> Result<Self> {
        let ok = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        match full.split_once('/') {
            Some((o, n)) if ok(o) && ok(n) => Ok(Repo {
                owner: o.into(),
                name: n.into(),
            }),
            _ => Err(Error::Invalid(format!("repository `{full}`"))),
        }
    }
    fn path(&self) -> String {
        format!("/repos/{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Created {
    pub number: u64,
    pub html_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CommentRef {
    pub id: u64,
    pub html_url: String,
}

#[derive(Deserialize)]
struct RefObject {
    sha: String,
}
#[derive(Deserialize)]
struct RefBody {
    object: RefObject,
}
#[derive(Deserialize)]
struct Ignored {}

#[derive(Serialize)]
struct IssueBody<'a> {
    title: &'a str,
    body: &'a str,
    labels: &'a [String],
}
#[derive(Serialize)]
struct CommentBody<'a> {
    body: &'a str,
}
#[derive(Serialize)]
struct LabelsBody<'a> {
    labels: &'a [String],
}
#[derive(Serialize)]
struct StateBody {
    state: &'static str,
}
#[derive(Serialize)]
struct RefCreate<'a> {
    #[serde(rename = "ref")]
    name: String,
    sha: &'a str,
}
#[derive(Serialize)]
struct PullBody<'a> {
    title: &'a str,
    head: &'a str,
    base: &'a str,
    body: &'a str,
}

/// The relay's GitHub driver.
pub struct GithubRelay {
    crab: Octocrab,
}

impl fmt::Debug for GithubRelay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GithubRelay(..)")
    }
}

impl GithubRelay {
    /// Authenticates as the App installation. `base_uri` is `https://api.github.com`
    /// except in tests.
    pub fn new(creds: &AppCredentials, base_uri: &str) -> Result<Self> {
        let app = Octocrab::builder()
            .base_uri(base_uri)
            .map_err(api)?
            .app_from_rsa_pem(AppId(creds.app_id), creds.private_key_pem.0.as_bytes())
            .map_err(api)?
            .build()
            .map_err(api)?;
        let crab = app
            .installation(InstallationId(creds.installation_id))
            .map_err(api)?;
        Ok(GithubRelay { crab })
    }

    /// Metadata read: the commit a branch points at.
    pub async fn branch_head(&self, repo: &Repo, branch: &str) -> Result<String> {
        check_ref_part(branch)?;
        let r: RefBody = self
            .crab
            .get(
                format!("{}/git/ref/heads/{branch}", repo.path()),
                None::<&()>,
            )
            .await
            .map_err(api)?;
        Ok(r.object.sha)
    }

    /// Contents write: a new `cforge/...` branch at `sha`.
    pub async fn branch_create(&self, repo: &Repo, branch: &str, sha: &str) -> Result<()> {
        check_ref_part(branch)?;
        if !branch.starts_with(RELAY_BRANCH_PREFIX) {
            return Err(Error::Invalid(format!(
                "relay branches start with `{RELAY_BRANCH_PREFIX}`"
            )));
        }
        let body = RefCreate {
            name: format!("refs/heads/{branch}"),
            sha,
        };
        let _: Ignored = self
            .crab
            .post(format!("{}/git/refs", repo.path()), Some(&body))
            .await
            .map_err(api)?;
        Ok(())
    }

    pub async fn issue_create(
        &self,
        repo: &Repo,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<Created> {
        self.crab
            .post(
                format!("{}/issues", repo.path()),
                Some(&IssueBody {
                    title,
                    body,
                    labels,
                }),
            )
            .await
            .map_err(api)
    }

    pub async fn comment(&self, repo: &Repo, number: u64, body: &str) -> Result<CommentRef> {
        self.crab
            .post(
                format!("{}/issues/{number}/comments", repo.path()),
                Some(&CommentBody { body }),
            )
            .await
            .map_err(api)
    }

    pub async fn labels_add(&self, repo: &Repo, number: u64, labels: &[String]) -> Result<()> {
        let _: serde_json::Value = self
            .crab
            .post(
                format!("{}/issues/{number}/labels", repo.path()),
                Some(&LabelsBody { labels }),
            )
            .await
            .map_err(api)?;
        Ok(())
    }

    pub async fn issue_close(&self, repo: &Repo, number: u64) -> Result<()> {
        let _: Ignored = self
            .crab
            .patch(
                format!("{}/issues/{number}", repo.path()),
                Some(&StateBody { state: "closed" }),
            )
            .await
            .map_err(api)?;
        Ok(())
    }

    pub async fn pull_open(
        &self,
        repo: &Repo,
        title: &str,
        head: &str,
        base: &str,
        body: &str,
    ) -> Result<Created> {
        check_ref_part(base)?;
        self.crab
            .post(
                format!("{}/pulls", repo.path()),
                Some(&PullBody {
                    title,
                    head,
                    base,
                    body,
                }),
            )
            .await
            .map_err(api)
    }

    pub async fn pull_close(&self, repo: &Repo, number: u64) -> Result<()> {
        let _: Ignored = self
            .crab
            .patch(
                format!("{}/pulls/{number}", repo.path()),
                Some(&StateBody { state: "closed" }),
            )
            .await
            .map_err(api)?;
        Ok(())
    }
}

fn check_ref_part(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("..")
        && !name.contains("//")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("ref name `{name}`")))
    }
}
