//! The live GitLab `HeadHost` of the pull request heads (Bridge design §3.3, build task B8).
//!
//! Writes that the GitLab driver (`cglb`) offers go through it: `fork_into_relay_subgroup`,
//! `merge_request_note` and `merge_request_close`. Two things the driver has no call for are done here with `curl`
//! (the token travels in a header file of a private temporary directory, never in an argument or a message):
//! the reads (does the subgroup or fork exist, which merge request has this head) and opening a merge request
//! from the relay fork to the copy (`target_project_id`; the driver's `merge_request_open` stays inside one
//! project). Heads are pushed by [`cfrg::relay::heads::GitCli`] to the fork's URL.

use cfrg::{
    model::Forge,
    relay::{
        forge::ForgeError,
        heads::{HeadHost, ReqRef, ReqState, Site},
    },
};
use serde_json::Value;
use std::{
    cell::RefCell, collections::BTreeMap, fs, process::Command, rc::Rc, thread, time::Duration,
};
use tokio::runtime::Runtime;

/// Percent-encodes a path for a URL path segment.
pub fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A small authenticated REST client over `curl`.
pub struct Rest {
    base: String,
    dir: tempfile::TempDir,
}

fn http_error(code: u16, body: &str) -> ForgeError {
    let detail = format!(
        "http {code}: {}",
        body.chars().take(200).collect::<String>()
    );
    match code {
        401 => ForgeError::Refused(cfrg::relay::forge::Refusal::Unauthorized, detail),
        403 => ForgeError::Refused(cfrg::relay::forge::Refusal::Forbidden, detail),
        429 => ForgeError::Refused(cfrg::relay::forge::Refusal::RateLimited, detail),
        500..=599 => ForgeError::Unavailable(detail),
        _ => ForgeError::Rejected(detail),
    }
}

impl Rest {
    /// `header` is the full authentication header line, e.g. `PRIVATE-TOKEN: ...`.
    pub fn new(base: &str, header: &str) -> Result<Self, ForgeError> {
        let dir = tempfile::tempdir().map_err(|e| ForgeError::Unavailable(e.to_string()))?;
        fs::write(dir.path().join("h"), format!("{header}\n"))
            .map_err(|e| ForgeError::Unavailable(e.to_string()))?;
        Ok(Self {
            base: base.trim_end_matches('/').into(),
            dir,
        })
    }

    /// Returns the status and the JSON body (`Null` when empty).
    pub fn call(
        &self,
        method: &str,
        path: &str,
        form: &[(&str, &str)],
    ) -> Result<(u16, Value), ForgeError> {
        let mut cmd = Command::new("curl");
        cmd.args([
            "-sS",
            "--max-time",
            "30",
            "-X",
            method,
            "-w",
            "\n%{http_code}",
            "-H",
        ]);
        cmd.arg(format!("@{}", self.dir.path().join("h").display()));
        for (k, v) in form {
            cmd.arg("--data-urlencode").arg(format!("{k}={v}"));
        }
        cmd.arg(format!("{}/{path}", self.base));
        let out = cmd
            .output()
            .map_err(|e| ForgeError::Unavailable(e.to_string()))?;
        if !out.status.success() {
            return Err(ForgeError::Unavailable("curl failed".into()));
        }
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
        let code: u16 = code.trim().parse().unwrap_or(0);
        let json = serde_json::from_str(body).unwrap_or(Value::Null);
        Ok((code, json))
    }

    /// A read or write that must succeed; 404 is `None`.
    pub fn call_opt(
        &self,
        method: &str,
        path: &str,
        form: &[(&str, &str)],
    ) -> Result<Option<Value>, ForgeError> {
        match self.call(method, path, form)? {
            (404, _) => Ok(None),
            (200..=299, v) => Ok(Some(v)),
            (c, v) => Err(http_error(c, &v.to_string())),
        }
    }
}

pub struct GitlabHeads {
    pub rt: Rc<Runtime>,
    pub driver: cglb::relay::GitlabRelay,
    pub rest: Rest,
    /// Host for git URLs, normally `gitlab.com`.
    pub host: String,
    token: String,
    ids: RefCell<BTreeMap<String, u64>>,
}

impl GitlabHeads {
    pub fn new(
        rt: Rc<Runtime>,
        driver: cglb::relay::GitlabRelay,
        host: &str,
        token: &str,
    ) -> Result<Self, ForgeError> {
        let rest = Rest::new(
            &format!("https://{host}/api/v4"),
            &format!("PRIVATE-TOKEN: {token}"),
        )?;
        Ok(Self {
            rt,
            driver,
            rest,
            host: host.into(),
            token: token.into(),
            ids: RefCell::default(),
        })
    }

    fn path(p: &str) -> Result<cglb::relay::Path, ForgeError> {
        cglb::relay::Path::parse(p).map_err(|e| ForgeError::Rejected(e.to_string()))
    }

    fn fail(e: impl std::fmt::Display) -> ForgeError {
        ForgeError::Unavailable(e.to_string())
    }

    fn project_id(&self, path: &str) -> Result<Option<u64>, ForgeError> {
        if let Some(id) = self.ids.borrow().get(path) {
            return Ok(Some(*id));
        }
        let v = self
            .rest
            .call_opt("GET", &format!("projects/{}", enc(path)), &[])?;
        let id = v.and_then(|v| v["id"].as_u64());
        if let Some(id) = id {
            self.ids.borrow_mut().insert(path.into(), id);
        }
        Ok(id)
    }

    fn state_of(v: &Value) -> ReqState {
        match v["state"].as_str() {
            Some("merged") => ReqState::Merged,
            Some("closed") => ReqState::Closed,
            _ => ReqState::Open,
        }
    }
}

impl HeadHost for GitlabHeads {
    fn forge(&self) -> Forge {
        Forge::Gitlab
    }

    fn site(&mut self, copy_repo: &str) -> Result<Site, ForgeError> {
        let (group, name) = copy_repo
            .rsplit_once('/')
            .ok_or_else(|| ForgeError::Rejected("repository path".into()))?;
        let sub = format!("{group}/cforge");
        let fork = format!("{sub}/{name}");
        // The `<group>/cforge` subgroups are made once by the owner (least privilege: the relay account is
        // Maintainer there and nowhere else). A missing one is reported, never created by the relay.
        if self
            .rest
            .call_opt("GET", &format!("groups/{}", enc(&sub)), &[])?
            .is_none()
        {
            return Err(ForgeError::Rejected(format!(
                "the relay subgroup {sub} does not exist; the owner creates it once (the relay does not)"
            )));
        }
        if self.project_id(&fork)?.is_none() {
            let (project, group) = (Self::path(copy_repo)?, Self::path(group)?);
            self.rt
                .block_on(self.driver.fork_into_relay_subgroup(&project, &group))
                .map_err(Self::fail)?;
            // The fork is an import: wait, bounded, until it is finished.
            for _ in 0..60 {
                let v = self
                    .rest
                    .call_opt("GET", &format!("projects/{}", enc(&fork)), &[])?;
                if matches!(
                    v.as_ref().and_then(|v| v["import_status"].as_str()),
                    Some("finished" | "none")
                ) {
                    break;
                }
                thread::sleep(Duration::from_secs(1));
            }
            if self.project_id(&fork)?.is_none() {
                return Err(ForgeError::Unavailable(
                    "the relay fork did not appear".into(),
                ));
            }
        }
        Ok(Site {
            push_url: format!("https://oauth2:{}@{}/{fork}.git", self.token, self.host),
            repo: fork,
        })
    }

    fn branch_tip(&mut self, site: &Site, branch: &str) -> Result<Option<String>, ForgeError> {
        let r = self.rest.call_opt(
            "GET",
            &format!(
                "projects/{}/repository/branches/{}",
                enc(&site.repo),
                enc(branch)
            ),
            &[],
        )?;
        Ok(r.and_then(|v| v["commit"]["id"].as_str().map(str::to_owned)))
    }

    fn find_request(
        &mut self,
        copy_repo: &str,
        site: &Site,
        branch: &str,
    ) -> Result<Option<ReqRef>, ForgeError> {
        let Some(fork_id) = self.project_id(&site.repo)? else {
            return Ok(None);
        };
        let q = format!(
            "projects/{}/merge_requests?source_branch={}&state=all&order_by=created_at&sort=desc&per_page=20",
            enc(copy_repo),
            enc(branch)
        );
        let list = self.rest.call_opt("GET", &q, &[])?.unwrap_or(Value::Null);
        Ok(list
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["source_project_id"].as_u64() == Some(fork_id))
            .map(|m| ReqRef {
                number: m["iid"].as_u64().unwrap_or(0),
                url: m["web_url"].as_str().unwrap_or("").to_owned(),
                state: Self::state_of(m),
            }))
    }

    fn open_request(
        &mut self,
        copy_repo: &str,
        site: &Site,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<ReqRef, ForgeError> {
        let target = self
            .project_id(copy_repo)?
            .ok_or_else(|| ForgeError::Rejected("copy not found".into()))?;
        let target = target.to_string();
        let form = [
            ("source_branch", branch),
            ("target_branch", base),
            ("target_project_id", target.as_str()),
            ("title", title),
            ("description", body),
        ];
        let v = self
            .rest
            .call_opt(
                "POST",
                &format!("projects/{}/merge_requests", enc(&site.repo)),
                &form,
            )?
            .ok_or_else(|| ForgeError::Rejected("relay fork not found".into()))?;
        Ok(ReqRef {
            number: v["iid"].as_u64().unwrap_or(0),
            url: v["web_url"].as_str().unwrap_or("").to_owned(),
            state: ReqState::Open,
        })
    }

    fn request_state(&mut self, repo: &str, number: u64) -> Result<Option<ReqState>, ForgeError> {
        let v = self.rest.call_opt(
            "GET",
            &format!("projects/{}/merge_requests/{number}", enc(repo)),
            &[],
        )?;
        Ok(v.map(|v| Self::state_of(&v)))
    }

    fn close_request(&mut self, repo: &str, number: u64, note: &str) -> Result<(), ForgeError> {
        if self.request_state(repo, number)? != Some(ReqState::Open) {
            return Ok(());
        }
        let project = Self::path(repo)?;
        self.rt
            .block_on(self.driver.merge_request_note(&project, number, note))
            .map_err(Self::fail)?;
        self.rt
            .block_on(self.driver.merge_request_close(&project, number))
            .map_err(Self::fail)
    }

    fn delete_branch(&mut self, site: &Site, branch: &str) -> Result<(), ForgeError> {
        self.rest
            .call_opt(
                "DELETE",
                &format!(
                    "projects/{}/repository/branches/{}",
                    enc(&site.repo),
                    enc(branch)
                ),
                &[],
            )
            .map(|_| ())
    }
}

// ---------------------------------------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------------------------------------

/// The GitHub head host: the head is the branch `cforge/<origin>-<n>` inside the copy. Reads, the branch
/// delete and the push use the installation token; the pull request is opened and closed by the `cghb`
/// driver.
pub struct GithubHeads {
    pub rt: Rc<Runtime>,
    pub driver: cghb::relay::GithubRelay,
    pub rest: Rest,
    token: String,
}

impl GithubHeads {
    /// Takes the installation token from the driver (octocrab authenticates as the App) and builds the host.
    pub fn new(rt: Rc<Runtime>, driver: cghb::relay::GithubRelay) -> Result<Self, ForgeError> {
        let token = rt
            .block_on(driver.installation_token())
            .map_err(|e| ForgeError::Unavailable(e.to_string()))?
            .expose()
            .to_owned();
        let rest = Rest::new(
            "https://api.github.com",
            &format!("Authorization: Bearer {token}"),
        )?;
        Ok(Self {
            rt,
            driver,
            rest,
            token,
        })
    }

    fn repo(r: &str) -> Result<cghb::relay::Repo, ForgeError> {
        cghb::relay::Repo::parse(r).map_err(|e| ForgeError::Rejected(e.to_string()))
    }

    fn state_of(v: &Value) -> ReqState {
        if !v["merged_at"].is_null() || v["merged"].as_bool() == Some(true) {
            ReqState::Merged
        } else if v["state"].as_str() == Some("closed") {
            ReqState::Closed
        } else {
            ReqState::Open
        }
    }
}

impl HeadHost for GithubHeads {
    fn forge(&self) -> Forge {
        Forge::Github
    }

    fn site(&mut self, copy_repo: &str) -> Result<Site, ForgeError> {
        Self::repo(copy_repo)?;
        Ok(Site {
            repo: copy_repo.into(),
            push_url: format!(
                "https://x-access-token:{}@github.com/{copy_repo}.git",
                self.token
            ),
        })
    }

    fn branch_tip(&mut self, site: &Site, branch: &str) -> Result<Option<String>, ForgeError> {
        let r = self.rest.call_opt(
            "GET",
            &format!("repos/{}/git/ref/heads/{branch}", site.repo),
            &[],
        )?;
        Ok(r.and_then(|v| v["object"]["sha"].as_str().map(str::to_owned)))
    }

    fn find_request(
        &mut self,
        copy_repo: &str,
        _site: &Site,
        branch: &str,
    ) -> Result<Option<ReqRef>, ForgeError> {
        let owner = copy_repo.split('/').next().unwrap_or("");
        let q = format!(
            "repos/{copy_repo}/pulls?state=all&sort=created&direction=desc&per_page=10&head={}",
            enc(&format!("{owner}:{branch}"))
        );
        let list = self.rest.call_opt("GET", &q, &[])?.unwrap_or(Value::Null);
        Ok(list.as_array().and_then(|l| l.first()).map(|m| ReqRef {
            number: m["number"].as_u64().unwrap_or(0),
            url: m["html_url"].as_str().unwrap_or("").to_owned(),
            state: Self::state_of(m),
        }))
    }

    fn open_request(
        &mut self,
        copy_repo: &str,
        _site: &Site,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<ReqRef, ForgeError> {
        let repo = Self::repo(copy_repo)?;
        let c = self
            .rt
            .block_on(self.driver.pull_open(&repo, title, branch, base, body))
            .map_err(|e| ForgeError::Unavailable(e.to_string()))?;
        Ok(ReqRef {
            number: c.number,
            url: c.html_url,
            state: ReqState::Open,
        })
    }

    fn request_state(&mut self, repo: &str, number: u64) -> Result<Option<ReqState>, ForgeError> {
        let v = self
            .rest
            .call_opt("GET", &format!("repos/{repo}/pulls/{number}"), &[])?;
        Ok(v.map(|v| Self::state_of(&v)))
    }

    fn close_request(&mut self, repo: &str, number: u64, note: &str) -> Result<(), ForgeError> {
        if self.request_state(repo, number)? != Some(ReqState::Open) {
            return Ok(());
        }
        let r = Self::repo(repo)?;
        self.rt
            .block_on(self.driver.comment(&r, number, note))
            .map_err(|e| ForgeError::Unavailable(e.to_string()))?;
        self.rt
            .block_on(self.driver.pull_close(&r, number))
            .map_err(|e| ForgeError::Unavailable(e.to_string()))
    }

    fn delete_branch(&mut self, site: &Site, branch: &str) -> Result<(), ForgeError> {
        match self.rest.call(
            "DELETE",
            &format!("repos/{}/git/refs/heads/{branch}", site.repo),
            &[],
        )? {
            (200..=299 | 404 | 422, _) => Ok(()),
            (c, v) => Err(http_error(c, &v.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_keeps_slashes_out_of_paths() {
        assert_eq!(enc("a/b c"), "a%2Fb%20c");
    }
}
