//! Typed write capabilities for the relay on Forgejo, through the `forgejo-api`
//! client crate: issues, notice issues (label `contribution`) and
//! `workflow_dispatch`. There is no method that takes a route, a verb or a
//! free-form body. The API is async.

use std::collections::BTreeMap;
use std::fmt;

use forgejo_api::structs::{
    CreateIssueCommentOption, CreateIssueOption, DispatchWorkflowOption, EditIssueOption,
    IssueLabelsOption,
};
use forgejo_api::{Auth, Forgejo};

/// Label that marks a notice issue.
pub const NOTICE_LABEL: &str = "contribution";

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
            Error::Api(m) => write!(f, "forgejo api: {m}"),
        }
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

fn api(e: impl std::error::Error) -> Error {
    // The error text plus its causes; the client never puts a credential in either.
    let mut text = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        text.push_str(": ");
        text.push_str(&c.to_string());
        cause = c.source();
    }
    Error::Api(text)
}

fn missing(what: &str) -> Error {
    Error::Api(format!("response lacks {what}"))
}

/// The API token; prints as nothing.
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

/// `owner/name`, validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    owner: String,
    name: String,
}

fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl Repo {
    pub fn parse(full: &str) -> Result<Self> {
        match full.split_once('/') {
            Some((o, n)) if segment_ok(o) && segment_ok(n) => Ok(Repo {
                owner: o.into(),
                name: n.into(),
            }),
            _ => Err(Error::Invalid(format!("repository `{full}`"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub number: i64,
    pub html_url: String,
}

/// The relay's Forgejo driver.
pub struct ForgejoRelay {
    client: Forgejo,
}

impl fmt::Debug for ForgejoRelay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ForgejoRelay(..)")
    }
}

impl ForgejoRelay {
    /// `base` is the forge URL, for example `https://forge.corbet.ch`.
    pub fn new(base: &str, token: &Token) -> Result<Self> {
        let url = base
            .parse()
            .map_err(|_| Error::Invalid(format!("base url `{base}`")))?;
        let client = Forgejo::new(Auth::Token(&token.0), url).map_err(api)?;
        Ok(ForgejoRelay { client })
    }

    #[allow(deprecated)]
    pub async fn issue_create(&self, repo: &Repo, title: &str, body: &str) -> Result<Created> {
        let opt = CreateIssueOption {
            assignee: None,
            assignees: None,
            body: Some(body.to_string()),
            closed: None,
            due_date: None,
            labels: None,
            milestone: None,
            r#ref: None,
            title: title.to_string(),
        };
        let i = self
            .client
            .issue_create_issue(&repo.owner, &repo.name, opt)
            .await
            .map_err(api)?;
        Ok(Created {
            number: i.number.ok_or_else(|| missing("an issue number"))?,
            html_url: i.html_url.map(|u| u.to_string()).unwrap_or_default(),
        })
    }

    /// A notice issue: an issue carrying the `contribution` label.
    pub async fn notice_create(&self, repo: &Repo, title: &str, body: &str) -> Result<Created> {
        let c = self.issue_create(repo, title, body).await?;
        self.labels_add(repo, c.number, &[NOTICE_LABEL.to_string()])
            .await?;
        Ok(c)
    }

    pub async fn comment(&self, repo: &Repo, issue: i64, body: &str) -> Result<i64> {
        let c = self
            .client
            .issue_create_comment(
                &repo.owner,
                &repo.name,
                issue,
                CreateIssueCommentOption {
                    body: body.to_string(),
                    updated_at: None,
                },
            )
            .await
            .map_err(api)?;
        c.id.ok_or_else(|| missing("a comment id"))
    }

    /// Adds labels by name; they must exist in the repository.
    pub async fn labels_add(&self, repo: &Repo, issue: i64, labels: &[String]) -> Result<()> {
        self.client
            .issue_add_label(
                &repo.owner,
                &repo.name,
                issue,
                IssueLabelsOption {
                    labels: Some(labels.iter().map(|l| l.as_str().into()).collect()),
                    updated_at: None,
                },
            )
            .await
            .map_err(api)?;
        Ok(())
    }

    #[allow(deprecated)]
    pub async fn issue_close(&self, repo: &Repo, issue: i64) -> Result<()> {
        let opt = EditIssueOption {
            assignee: None,
            assignees: None,
            body: None,
            due_date: None,
            milestone: None,
            r#ref: None,
            state: Some("closed".to_string()),
            title: None,
            unset_due_date: None,
            updated_at: None,
        };
        self.client
            .issue_edit_issue(&repo.owner, &repo.name, issue, opt)
            .await
            .map_err(api)?;
        Ok(())
    }

    /// `workflow_dispatch` of one workflow file at a ref.
    pub async fn workflow_dispatch(
        &self,
        repo: &Repo,
        workflow_file: &str,
        git_ref: &str,
        inputs: &BTreeMap<String, String>,
    ) -> Result<()> {
        if !segment_ok(workflow_file) {
            return Err(Error::Invalid(format!("workflow file `{workflow_file}`")));
        }
        self.client
            .dispatch_workflow(
                &repo.owner,
                &repo.name,
                workflow_file,
                DispatchWorkflowOption {
                    inputs: Some(inputs.clone()),
                    r#ref: git_ref.to_string(),
                    return_run_info: None,
                },
            )
            .await
            .map_err(api)?;
        Ok(())
    }
}
