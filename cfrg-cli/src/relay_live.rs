//! The live forges of `cfrg relay`: the credentials, the one runtime, and the adapters of the three B3 drivers.
//!
//! Credentials come ONLY from systemd credentials (`$CREDENTIALS_DIRECTORY/<name>`, names in `docs/relay.md`); there is
//! no environment variable, no file path argument and no fallback to any personal token. A forge whose credential is
//! missing or unreadable gets no adapter: the engine treats it as an outage (RF1), and one log line on standard error
//! names the forge and the credential file (never a value).
//!
//! The drivers (B3) offer the write capabilities only (create an issue, comment, add labels, close). Everything else
//! the engine asks of a forge (reads, comment edits, deletions, marker search, maintainer lookup) answers
//! `Unavailable("... not implemented by the driver")`: the intent stays pending and nothing is written twice.
use cfrg::{
    model::Forge,
    relay::{
        forge::{Created, ForgeError, NewObject, ReadOutcome, RouterForge},
        merge::Patch,
        model::{Actor, IssueState, ObjectRef, RouterConfig},
        Forges,
    },
};
use std::{collections::BTreeMap, fs, path::PathBuf, rc::Rc};
use tokio::runtime::Runtime;

/// The fixed credential names (files in `$CREDENTIALS_DIRECTORY`).
pub const FORGEJO_TOKEN: &str = "forgejo-token";
pub const GITLAB_TOKEN: &str = "gitlab-token";
pub const GITHUB_APP_ID: &str = "github-app-id";
pub const GITHUB_INSTALLATION_ID: &str = "github-installation-id";
pub const GITHUB_APP_KEY: &str = "github-app-key";

/// Reads one credential; `None` for a missing, unreadable or empty one. Never a value in a message.
fn credential(dir: &Option<PathBuf>, name: &str) -> Option<String> {
    let dir = dir.as_ref()?;
    let text = fs::read_to_string(dir.join(name)).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn refused(forge: &str, names: &[&str]) {
    eprintln!(
        "relay: {forge} refused: credential {} is not provided to this unit; no request is made to {forge}",
        names.join(", ")
    );
}

/// The one small current-thread runtime of the process.
pub fn runtime() -> cfrg::Result<Rc<Runtime>> {
    Ok(Rc::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?,
    ))
}

/// Adapters for every forge whose credential is present.
pub fn forges(rt: &Rc<Runtime>, config: &RouterConfig, forgejo_origin: &str) -> Forges {
    forges_in(
        &std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from),
        rt,
        config,
        forgejo_origin,
    )
}

fn forges_in(
    dir: &Option<PathBuf>,
    rt: &Rc<Runtime>,
    config: &RouterConfig,
    forgejo_origin: &str,
) -> Forges {
    let mut forges = Forges::new();
    // repository id -> path, per forge: the engine names repositories by immutable id, the drivers by path.
    let mut paths: BTreeMap<Forge, BTreeMap<String, String>> = BTreeMap::new();
    for set in &config.repos {
        for (forge, member) in &set.members {
            paths
                .entry(*forge)
                .or_default()
                .insert(member.id.clone(), member.path.clone());
        }
    }
    let paths_of = |forge: Forge| paths.get(&forge).cloned().unwrap_or_default();

    match credential(dir, FORGEJO_TOKEN) {
        Some(token) => {
            match cfgj::relay::ForgejoRelay::new(forgejo_origin, &cfgj::relay::Token::new(token)) {
                Ok(driver) => forges.insert(Box::new(Forgejo {
                    rt: rt.clone(),
                    driver,
                    paths: paths_of(Forge::Forgejo),
                })),
                Err(why) => eprintln!("relay: forgejo refused: {why}"),
            }
        }
        None => refused("forgejo", &[FORGEJO_TOKEN]),
    }
    match credential(dir, GITLAB_TOKEN) {
        Some(token) => match rt.block_on(cglb::relay::GitlabRelay::new(
            "gitlab.com",
            &cglb::relay::Token::new(token),
            false,
        )) {
            Ok(driver) => forges.insert(Box::new(Gitlab {
                rt: rt.clone(),
                driver,
                paths: paths_of(Forge::Gitlab),
            })),
            Err(why) => eprintln!("relay: gitlab refused: {why}"),
        },
        None => refused("gitlab", &[GITLAB_TOKEN]),
    }
    let app = (
        credential(dir, GITHUB_APP_ID),
        credential(dir, GITHUB_INSTALLATION_ID),
        credential(dir, GITHUB_APP_KEY),
    );
    match app {
        (Some(app_id), Some(installation_id), Some(key)) => {
            match (app_id.parse::<u64>(), installation_id.parse::<u64>()) {
                (Ok(app_id), Ok(installation_id)) => {
                    let creds = cghb::relay::AppCredentials {
                        app_id,
                        installation_id,
                        private_key_pem: cghb::relay::Redacted::new(key),
                    };
                    match rt.block_on(async { cghb::relay::GithubRelay::new(&creds, "https://api.github.com") }) {
                        Ok(driver) => forges.insert(Box::new(Github { rt: rt.clone(), driver, paths: paths_of(Forge::Github) })),
                        Err(why) => eprintln!("relay: github refused: {why}"),
                    }
                }
                _ => eprintln!("relay: github refused: {GITHUB_APP_ID} and {GITHUB_INSTALLATION_ID} must be numbers"),
            }
        }
        _ => refused(
            "github",
            &[GITHUB_APP_ID, GITHUB_INSTALLATION_ID, GITHUB_APP_KEY],
        ),
    }
    forges
}

fn missing(op: &str) -> ForgeError {
    ForgeError::Unavailable(format!("{op} is not implemented by the driver"))
}

fn invalid(e: impl std::fmt::Display, rejected: bool) -> ForgeError {
    if rejected {
        ForgeError::Rejected(e.to_string())
    } else {
        ForgeError::Unavailable(e.to_string())
    }
}

fn path_of(paths: &BTreeMap<String, String>, repo: &str) -> Result<String, ForgeError> {
    paths.get(repo).cloned().ok_or_else(|| {
        ForgeError::Rejected(format!(
            "repository {repo} is not a member of the relay set"
        ))
    })
}

fn snap_of(new: &NewObject) -> cfrg::relay::model::Snap {
    cfrg::relay::model::Snap {
        title: new.title.clone(),
        body: new.body.clone(),
        state: Some(IssueState::Open),
        labels: new.labels.iter().cloned().collect(),
        assignees: Default::default(),
    }
}

/// The write subset of a patch that every driver has: state `closed` and added labels. Anything else is not written.
fn patch_supported(patch: &Patch) -> Result<(), ForgeError> {
    let reopens = patch.state == Some(IssueState::Open);
    if patch.title.is_some()
        || patch.body.is_some()
        || reopens
        || !patch.labels_remove.is_empty()
        || !patch.assignees_add.is_empty()
        || !patch.assignees_remove.is_empty()
    {
        return Err(missing(
            "update (title, body, reopen, label removal, assignees)",
        ));
    }
    Ok(())
}

macro_rules! unsupported_reads {
    () => {
        fn read(&mut self, _: &ObjectRef) -> Result<ReadOutcome, ForgeError> {
            Err(missing("read"))
        }
        fn update_comment(&mut self, _: &ObjectRef, _: u64, _: &str) -> Result<(), ForgeError> {
            Err(missing("update_comment"))
        }
        fn delete_comment(&mut self, _: &ObjectRef, _: u64) -> Result<(), ForgeError> {
            Err(missing("delete_comment"))
        }
        fn delete_object(&mut self, _: &ObjectRef) -> Result<(), ForgeError> {
            Err(missing("delete_object"))
        }
        fn find_by_marker(&mut self, _: &str, _: &str) -> Result<Option<Created>, ForgeError> {
            Err(missing("find_by_marker"))
        }
        fn is_maintainer(&mut self, _: &str, _: &Actor) -> Result<Option<bool>, ForgeError> {
            Ok(None)
        }
    };
}

struct Github {
    rt: Rc<Runtime>,
    driver: cghb::relay::GithubRelay,
    paths: BTreeMap<String, String>,
}
impl Github {
    fn repo(&self, id: &str) -> Result<cghb::relay::Repo, ForgeError> {
        cghb::relay::Repo::parse(&path_of(&self.paths, id)?).map_err(|e| invalid(e, true))
    }
}
impl RouterForge for Github {
    fn forge(&self) -> Forge {
        Forge::Github
    }
    fn create_object(&mut self, repo: &str, new: &NewObject) -> Result<Created, ForgeError> {
        let r = self.repo(repo)?;
        let c = self
            .rt
            .block_on(
                self.driver
                    .issue_create(&r, &new.title, &new.body, &new.labels),
            )
            .map_err(|e| invalid(e, false))?;
        Ok(Created {
            number: c.number,
            url: c.html_url,
            snap: snap_of(new),
        })
    }
    fn update_object(&mut self, o: &ObjectRef, patch: &Patch) -> Result<(), ForgeError> {
        patch_supported(patch)?;
        let r = self.repo(&o.repo)?;
        if !patch.labels_add.is_empty() {
            let labels: Vec<String> = patch.labels_add.iter().cloned().collect();
            self.rt
                .block_on(self.driver.labels_add(&r, o.number, &labels))
                .map_err(|e| invalid(e, false))?;
        }
        if patch.state == Some(IssueState::Closed) {
            self.rt
                .block_on(self.driver.issue_close(&r, o.number))
                .map_err(|e| invalid(e, false))?;
        }
        Ok(())
    }
    fn create_comment(&mut self, o: &ObjectRef, body: &str) -> Result<u64, ForgeError> {
        let r = self.repo(&o.repo)?;
        Ok(self
            .rt
            .block_on(self.driver.comment(&r, o.number, body))
            .map_err(|e| invalid(e, false))?
            .id)
    }
    unsupported_reads!();
}

struct Gitlab {
    rt: Rc<Runtime>,
    driver: cglb::relay::GitlabRelay,
    paths: BTreeMap<String, String>,
}
impl Gitlab {
    fn project(&self, id: &str) -> Result<cglb::relay::Path, ForgeError> {
        cglb::relay::Path::parse(&path_of(&self.paths, id)?).map_err(|e| invalid(e, true))
    }
}
impl RouterForge for Gitlab {
    fn forge(&self) -> Forge {
        Forge::Gitlab
    }
    fn create_object(&mut self, repo: &str, new: &NewObject) -> Result<Created, ForgeError> {
        let p = self.project(repo)?;
        let c = self
            .rt
            .block_on(
                self.driver
                    .issue_create(&p, &new.title, &new.body, &new.labels),
            )
            .map_err(|e| invalid(e, false))?;
        Ok(Created {
            number: c.iid,
            url: c.web_url,
            snap: snap_of(new),
        })
    }
    fn update_object(&mut self, _: &ObjectRef, patch: &Patch) -> Result<(), ForgeError> {
        patch_supported(patch)?;
        Err(missing(
            "update (the GitLab driver has no label or close call for existing issues yet)",
        ))
    }
    fn create_comment(&mut self, o: &ObjectRef, body: &str) -> Result<u64, ForgeError> {
        let p = self.project(&o.repo)?;
        Ok(self
            .rt
            .block_on(self.driver.issue_note(&p, o.number, body))
            .map_err(|e| invalid(e, false))?
            .id)
    }
    unsupported_reads!();
}

struct Forgejo {
    rt: Rc<Runtime>,
    driver: cfgj::relay::ForgejoRelay,
    paths: BTreeMap<String, String>,
}
impl Forgejo {
    fn repo(&self, id: &str) -> Result<cfgj::relay::Repo, ForgeError> {
        cfgj::relay::Repo::parse(&path_of(&self.paths, id)?).map_err(|e| invalid(e, true))
    }
}
impl RouterForge for Forgejo {
    fn forge(&self) -> Forge {
        Forge::Forgejo
    }
    fn create_object(&mut self, repo: &str, new: &NewObject) -> Result<Created, ForgeError> {
        let r = self.repo(repo)?;
        let c = self
            .rt
            .block_on(self.driver.issue_create(&r, &new.title, &new.body))
            .map_err(|e| invalid(e, false))?;
        if !new.labels.is_empty() {
            self.rt
                .block_on(self.driver.labels_add(&r, c.number, &new.labels))
                .map_err(|e| invalid(e, false))?;
        }
        Ok(Created {
            number: c.number as u64,
            url: c.html_url,
            snap: snap_of(new),
        })
    }
    fn update_object(&mut self, o: &ObjectRef, patch: &Patch) -> Result<(), ForgeError> {
        patch_supported(patch)?;
        let r = self.repo(&o.repo)?;
        if !patch.labels_add.is_empty() {
            let labels: Vec<String> = patch.labels_add.iter().cloned().collect();
            self.rt
                .block_on(self.driver.labels_add(&r, o.number as i64, &labels))
                .map_err(|e| invalid(e, false))?;
        }
        if patch.state == Some(IssueState::Closed) {
            self.rt
                .block_on(self.driver.issue_close(&r, o.number as i64))
                .map_err(|e| invalid(e, false))?;
        }
        Ok(())
    }
    fn create_comment(&mut self, o: &ObjectRef, body: &str) -> Result<u64, ForgeError> {
        let r = self.repo(&o.repo)?;
        let id = self
            .rt
            .block_on(self.driver.comment(&r, o.number as i64, body))
            .map_err(|e| invalid(e, false))?;
        u64::try_from(id).map_err(|_| ForgeError::Rejected("negative comment id".into()))
    }
    unsupported_reads!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forge_without_its_credential_has_no_adapter() {
        let rt = runtime().unwrap();
        let cfg = RouterConfig::default();
        assert!(forges_in(&None, &rt, &cfg, "https://forge.example").is_empty());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FORGEJO_TOKEN), "  \n").unwrap(); // empty = missing
        std::fs::write(dir.path().join(GITHUB_APP_ID), "12").unwrap(); // the other two github files are absent
        assert!(forges_in(
            &Some(dir.path().to_path_buf()),
            &rt,
            &cfg,
            "https://forge.example"
        )
        .is_empty());
    }

    #[test]
    fn a_present_forgejo_token_gives_an_adapter_that_cannot_read_yet() {
        let rt = runtime().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FORGEJO_TOKEN), "t\n").unwrap();
        let mut f = forges_in(
            &Some(dir.path().to_path_buf()),
            &rt,
            &RouterConfig::default(),
            "https://forge.example",
        );
        let adapter = f.get(Forge::Forgejo).expect("forgejo adapter");
        let object = ObjectRef {
            forge: Forge::Forgejo,
            repo: "1".into(),
            kind: cfrg::relay::model::Kind::Issue,
            number: 1,
        };
        assert!(matches!(
            adapter.read(&object),
            Err(ForgeError::Unavailable(_))
        ));
        assert!(f.get(Forge::Gitlab).is_none() && f.get(Forge::Github).is_none());
    }
}
