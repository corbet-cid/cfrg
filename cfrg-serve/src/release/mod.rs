//! The release upload (operating model R45), ported from the Python bridge with its tests as the parity contract.
//!
//! A published Forgejo release goes to the GitHub and GitLab copies by API. The request is a hint, never an
//! instruction: the module is told "OWNER/REPO TAG" and reads everything else from Forgejo. Per copy, one pass:
//!
//! * read: the release of the tag on the primary; its checksum contract (one `SHA256SUMS`, or one `<asset>.sha256`
//!   per asset) must cover every asset
//! * compare: nothing to write is `in_sync` with ZERO writes
//! * gates before the first write: the tag is ON the copy at the same commit (this module never creates a tag and
//!   never pushes git: that is cfrg's, R2/R42), and CI is provably OFF on the copy (R6, R7); the repository is not a
//!   contributed project (R10, R11). A failed gate is a status, exit 75, nothing written
//! * write: every asset is first verified against the contract, then uploaded one at a time; an asset that exists
//!   with other content is NEVER replaced
//! * verify: read the copy back
//!
//! Statuses: `in_sync synced no_copy skipped refused` (done), `pending_tag blocked_ci rate_limited` (exit 75,
//! incomplete, not a fault), `failed` (exit 1). No retry loop anywhere: the next event completes a stopped copy.
mod api;
mod github;
mod gitlab;
mod primary;

pub use api::{Api, ApiError, Pacer};
pub use primary::Primary;

use crate::config::{Config, Credentials};
use cfrg::contributed::Contributed;
use reqwest::Client;
use serde::Serialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};

pub const SUM_FILES: [&str; 6] = [
    "SHA256SUMS",
    "SHA256SUMS.txt",
    "sha256sums.txt",
    "checksums.txt",
    "checksums.sha256",
    "CHECKSUMS",
];
/// A checksum file larger than this is not a checksum file.
pub const SMALL: u64 = 4 << 20;

pub fn log(event: &str, fields: Value) {
    let mut line = json!({"module": "release-upload", "event": event});
    if let (Some(line), Some(extra)) = (line.as_object_mut(), fields.as_object()) {
        for (k, v) in extra {
            line.insert(k.clone(), v.clone());
        }
    }
    println!("{line}");
}

/// `urllib.parse.quote(text, safe="")`.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn only(text: &str, first: impl Fn(u8) -> bool, rest: impl Fn(u8) -> bool, max: usize) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= max
        && first(bytes[0])
        && bytes[1..].iter().all(|b| rest(*b))
}

pub fn valid_tag(tag: &str) -> bool {
    only(
        tag,
        |b| b.is_ascii_alphanumeric(),
        |b| b.is_ascii_alphanumeric() || b"._+-".contains(&b),
        128,
    )
}

pub fn valid_asset_name(name: &str) -> bool {
    only(
        name,
        |b| b.is_ascii_alphanumeric(),
        |b| b.is_ascii_alphanumeric() || b"._+@=-".contains(&b),
        200,
    )
}

/// `OWNER/REPO`: no "." or ".." component; a leading "." is real (`.github`).
pub fn valid_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    let part = |p: &str| {
        !p.is_empty()
            && p != "."
            && p != ".."
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    };
    part(owner) && part(name)
}

#[derive(Debug)]
pub enum Stop {
    /// The primary release does not satisfy its own checksum contract: nothing is written anywhere.
    Contract(String),
    RateLimited(String),
    Io(String),
}

impl From<ApiError> for Stop {
    fn from(error: ApiError) -> Self {
        match error {
            ApiError::RateLimited(m) => Self::RateLimited(m),
            ApiError::Io(m) => Self::Io(m),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CopyResult {
    pub status: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assets: Option<usize>,
}

pub fn result(status: &str, detail: impl Into<String>) -> CopyResult {
    CopyResult {
        status: status.into(),
        detail: detail.into(),
        writes: None,
        assets: None,
    }
}

pub fn written(mut copy: CopyResult, writes: u64) -> CopyResult {
    copy.writes = Some(writes);
    copy
}

#[derive(Debug, Serialize)]
pub struct Outcome {
    pub repo: String,
    pub tag: String,
    pub exit: i32,
    pub copies: BTreeMap<String, CopyResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What a sync needs: configuration, credentials, the contributed index and the two HTTP clients.
pub struct Ctx {
    pub cfg: Config,
    pub creds: crate::config::Credentials,
    pub contributed: Contributed,
    /// Trusts ONLY the cluster CA bundle: the in-cluster Forgejo.
    pub forge_client: Client,
    /// The public roots: the providers.
    pub public_client: Client,
}

impl Ctx {
    pub fn new(cfg: Config, creds: Credentials) -> crate::config::Result<Self> {
        let extra: Vec<&str> = cfg.forge_hosts.iter().skip(1).map(String::as_str).collect();
        let contributed = match &cfg.placement {
            Some(path) => Contributed::load(Path::new(path), &extra)?,
            None => {
                let host = cfg.forge_hosts.first().cloned().unwrap_or_default();
                let document = format!(
                    r#"{{"native":{{"schema":1,"source":{{"origin":"https://{host}","token_env":"CFRG_FORGEJO_TOKEN"}},"repositories":[]}}}}"#
                );
                let placement = cfrg::native::Placement::from_document(document.as_bytes())?;
                Contributed::from_placement(&placement, &extra)?
            }
        };
        let public_client = crate::http::public_client()?;
        let forge_client = crate::http::forge_client(cfg.forge_ca.as_deref().map(Path::new))?;
        Ok(Self {
            cfg,
            creds,
            contributed,
            forge_client,
            public_client,
        })
    }

    /// The host the primary is known by (the first of `forge_hosts`).
    pub fn primary_host(&self) -> &str {
        self.cfg.forge_hosts.first().map_or("", String::as_str)
    }
}

pub async fn sync(ctx: &Ctx, repo: &str, tag: &str, dry: bool) -> Outcome {
    sync_for(ctx, repo, tag, dry, None).await
}

/// [`sync`] limited to one copy (`github` or `gitlab`); `None` = both.
pub async fn sync_for(ctx: &Ctx, repo: &str, tag: &str, dry: bool, only: Option<&str>) -> Outcome {
    let mut out = Outcome {
        repo: repo.to_owned(),
        tag: tag.to_owned(),
        exit: 0,
        copies: BTreeMap::new(),
        detail: None,
    };
    if !valid_repo(repo) || !valid_tag(tag) {
        out.exit = 1;
        out.detail = Some("refused: the repository or tag has an unexpected shape".into());
        return out;
    }
    // R10, R11: a contributed project's releases are not relayed, whatever the request says.
    if let Err(error) = ctx
        .contributed
        .refuse("release upload", ctx.primary_host(), repo)
    {
        out.detail = Some(error.to_string());
        log(
            "refused",
            json!({"repo": repo, "tag": tag, "detail": error.to_string()}),
        );
        return out;
    }
    let mut primary = Primary::new(ctx, repo, tag);
    match primary.read().await {
        Ok(None) => {}
        Ok(Some(skip)) => {
            log(
                "nothing-to-do",
                json!({"repo": repo, "tag": tag, "detail": skip}),
            );
            out.detail = Some(skip);
            return out;
        }
        Err(Stop::Contract(error)) => {
            log(
                "contract-failed",
                json!({"repo": repo, "tag": tag, "detail": error}),
            );
            out.exit = 1;
            out.detail = Some(format!(
                "the primary release does not satisfy its checksum contract: {error}"
            ));
            return out;
        }
        Err(Stop::RateLimited(m) | Stop::Io(m)) => {
            out.exit = 1;
            out.detail = Some(format!("the primary could not be read: {m}"));
            return out;
        }
    }
    out.copies.insert(
        "bitbucket".into(),
        result(
            "skipped",
            "git and tags only (R45: no download area on the free plan)",
        ),
    );
    for name in ["github", "gitlab"]
        .into_iter()
        .filter(|n| only.is_none_or(|o| o == *n))
    {
        let copy_path = ctx.cfg.copy_path(name, repo);
        let host = if name == "github" {
            "github.com".to_owned()
        } else {
            gitlab_host(&ctx.cfg)
        };
        let copy = if let Err(error) = ctx.contributed.refuse("release upload", &host, &copy_path) {
            result("refused", error.to_string())
        } else {
            let run = if name == "github" {
                github::sync(ctx, &mut primary, dry).await
            } else {
                gitlab::sync(ctx, &mut primary, dry).await
            };
            match run {
                Ok(copy) => copy,
                Err(Stop::RateLimited(m)) => result(
                    "rate_limited",
                    format!("{m}; stopped, the next hint completes it"),
                ),
                Err(Stop::Contract(m) | Stop::Io(m)) => result("failed", m),
            }
        };
        log(
            "copy",
            json!({"repo": repo, "tag": tag, "copy": name, "status": copy.status, "detail": copy.detail}),
        );
        out.copies.insert(name.into(), copy);
    }
    let has = |s: &str| out.copies.values().any(|c| c.status == s);
    out.exit = if has("failed") {
        1
    } else if has("pending_tag") || has("blocked_ci") || has("rate_limited") {
        75
    } else {
        0
    };
    out
}

fn gitlab_host(cfg: &Config) -> String {
    reqwest::Url::parse(&cfg.gitlab.api)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "gitlab.com".into())
}

/// Every item of a paginated GitLab list: GitLab answers 20 per page by default (the first live run saw 20 of 45).
pub async fn paged(api: &Api, path: &str, limit: usize) -> Result<Vec<Value>, Stop> {
    let joiner = if path.contains('?') { '&' } else { '?' };
    let mut items = Vec::new();
    for page in 1..=limit {
        let (status, chunk) = api
            .get_json(&format!("{path}{joiner}per_page=100&page={page}"))
            .await?;
        let Some(Value::Array(chunk)) = chunk.filter(|_| status == 200) else {
            break;
        };
        let more = chunk.len() >= 100;
        items.extend(chunk);
        if !more {
            break;
        }
    }
    Ok(items)
}

pub fn norm_text(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_owned()
}
