//! Typed write capabilities for the relay on GitLab.com (Free tier), through the
//! `gitlab` client crate and a service-account token (Bridge design, section 8).
//!
//! Only the operations below exist: issues, notes, labels, merge requests, and
//! forks into a `<group>/cforge` subgroup. There is no method that takes a route,
//! a verb or a free-form body. The API is async.

use std::borrow::Cow;
use std::fmt;

use gitlab::api::common::NameOrId;
use gitlab::api::endpoint_prelude::{Endpoint, FormParams, Method};
use gitlab::api::projects::issues::IssueStateEvent;
use gitlab::api::{groups, projects, AsyncQuery};
use gitlab::{AsyncGitlab, Gitlab};
use serde::Deserialize;

/// Name of the subgroup that holds relay forks.
pub const RELAY_SUBGROUP: &str = "cforge";

#[derive(Debug)]
pub enum Error {
    /// An argument failed validation before any request was made.
    Invalid(String),
    /// The request failed. The text never contains the token.
    Api(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(m) => write!(f, "invalid argument: {m}"),
            Error::Api(m) => write!(f, "gitlab api: {m}"),
        }
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

fn api(e: impl fmt::Display) -> Error {
    Error::Api(e.to_string())
}

/// The service-account token; prints as nothing.
#[derive(Clone)]
pub struct Token(String);
impl Token {
    pub fn new(value: impl Into<String>) -> Self {
        Token(value.into())
    }
}
impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// A project or group path such as `group/sub/project`, validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path(String);

impl Path {
    pub fn parse(full: &str) -> Result<Self> {
        let seg = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if full.split('/').all(seg) {
            Ok(Path(full.to_string()))
        } else {
            Err(Error::Invalid(format!("path `{full}`")))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Created {
    pub iid: u64,
    pub web_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NoteRef {
    pub id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ForkRef {
    pub id: u64,
    pub path_with_namespace: String,
    pub web_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GroupRef {
    pub id: u64,
    pub full_path: String,
}

#[derive(Deserialize)]
struct Ignored {}

/// `POST /projects/:id/fork` into a namespace path; the `gitlab` crate has no
/// endpoint for it.
struct ForkProject<'a> {
    project: NameOrId<'a>,
    namespace_path: &'a str,
}

impl Endpoint for ForkProject<'_> {
    fn method(&self) -> Method {
        Method::POST
    }
    fn endpoint(&self) -> Cow<'static, str> {
        format!("projects/{}/fork", self.project).into()
    }
    fn body(&self) -> std::result::Result<Option<(&'static str, Vec<u8>)>, gitlab::api::BodyError> {
        let mut p = FormParams::default();
        p.push("namespace_path", self.namespace_path);
        p.into_body()
    }
}

/// The relay's GitLab driver.
pub struct GitlabRelay {
    client: AsyncGitlab,
}

impl fmt::Debug for GitlabRelay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GitlabRelay(..)")
    }
}

impl GitlabRelay {
    /// `host` is `gitlab.com`. `insecure` (plain HTTP) is for tests only.
    pub async fn new(host: &str, token: &Token, insecure: bool) -> Result<Self> {
        let mut b = Gitlab::builder(host, token.0.clone());
        if insecure {
            b.insecure();
        }
        let client = b.build_async().await.map_err(api)?;
        Ok(GitlabRelay { client })
    }

    pub async fn issue_create(
        &self,
        project: &Path,
        title: &str,
        description: &str,
        labels: &[String],
    ) -> Result<Created> {
        let ep = projects::issues::CreateIssue::builder()
            .project(project.as_str())
            .title(title)
            .description(description)
            .labels(labels.iter().map(String::as_str))
            .build()
            .map_err(api)?;
        ep.query_async(&self.client).await.map_err(api)
    }

    pub async fn issue_note(&self, project: &Path, issue: u64, body: &str) -> Result<NoteRef> {
        let ep = projects::issues::notes::CreateIssueNote::builder()
            .project(project.as_str())
            .issue(issue)
            .body(body)
            .build()
            .map_err(api)?;
        ep.query_async(&self.client).await.map_err(api)
    }

    pub async fn issue_close(&self, project: &Path, issue: u64) -> Result<()> {
        let ep = projects::issues::EditIssue::builder()
            .project(project.as_str())
            .issue(issue)
            .state_event(IssueStateEvent::Close)
            .build()
            .map_err(api)?;
        let _: Ignored = ep.query_async(&self.client).await.map_err(api)?;
        Ok(())
    }

    /// A project label with a hex colour such as `#f29513`.
    pub async fn label_create(&self, project: &Path, name: &str, color: &str) -> Result<()> {
        let ep = projects::labels::CreateLabel::builder()
            .project(project.as_str())
            .name(name)
            .color(color)
            .build()
            .map_err(api)?;
        let _: Ignored = ep.query_async(&self.client).await.map_err(api)?;
        Ok(())
    }

    /// Opens a merge request. `source_project` is the relay fork's id when the
    /// head lives in a fork.
    pub async fn merge_request_open(
        &self,
        project: &Path,
        source_branch: &str,
        target_branch: &str,
        title: &str,
        description: &str,
    ) -> Result<Created> {
        let ep = projects::merge_requests::CreateMergeRequest::builder()
            .project(project.as_str())
            .source_branch(source_branch)
            .target_branch(target_branch)
            .title(title)
            .description(description)
            .build()
            .map_err(api)?;
        ep.query_async(&self.client).await.map_err(api)
    }

    pub async fn merge_request_note(
        &self,
        project: &Path,
        merge_request: u64,
        body: &str,
    ) -> Result<NoteRef> {
        let ep = projects::merge_requests::notes::CreateMergeRequestNote::builder()
            .project(project.as_str())
            .merge_request(merge_request)
            .body(body)
            .build()
            .map_err(api)?;
        ep.query_async(&self.client).await.map_err(api)
    }

    pub async fn merge_request_close(&self, project: &Path, merge_request: u64) -> Result<()> {
        let ep = projects::merge_requests::EditMergeRequest::builder()
            .project(project.as_str())
            .merge_request(merge_request)
            .state_event(projects::merge_requests::MergeRequestStateEvent::Close)
            .build()
            .map_err(api)?;
        let _: Ignored = ep.query_async(&self.client).await.map_err(api)?;
        Ok(())
    }

    /// Creates `<group>/cforge` under a top-level group. Idempotent callers read
    /// first with [`GitlabRelay::group`].
    pub async fn subgroup_create(&self, parent: &Path) -> Result<GroupRef> {
        let parent_ref = self.group(parent).await?;
        let ep = groups::CreateGroup::builder()
            .name(RELAY_SUBGROUP)
            .path(RELAY_SUBGROUP)
            .parent_id(parent_ref.id)
            .build()
            .map_err(api)?;
        ep.query_async(&self.client).await.map_err(api)
    }

    /// Metadata read of a group (used to find a parent or an existing subgroup).
    pub async fn group(&self, path: &Path) -> Result<GroupRef> {
        let ep = groups::Group::builder()
            .group(path.as_str())
            .build()
            .map_err(api)?;
        ep.query_async(&self.client).await.map_err(api)
    }

    /// Forks `project` into `<group>/cforge`.
    pub async fn fork_into_relay_subgroup(&self, project: &Path, group: &Path) -> Result<ForkRef> {
        let ns = format!("{}/{RELAY_SUBGROUP}", group.as_str());
        let ep = ForkProject {
            project: NameOrId::from(project.as_str()),
            namespace_path: &ns,
        };
        ep.query_async(&self.client).await.map_err(api)
    }
}
