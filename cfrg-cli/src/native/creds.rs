//! The credentials of `cfrg native`: one file per credential in the credentials directory (the format the relay and
//! `cfrg-serve` read), the environment variable named by the endpoint only as a fallback for a hand run.
//!
//! | `token_env` | file in the directory |
//! |---|---|
//! | `CFRG_FORGEJO_TOKEN` | `forgejo-token` |
//! | `CFRG_GITLAB_TOKEN` | `gitlab-token` |
//! | `CFRG_GITHUB_TOKEN` | `github-token`, else an installation token minted from `github-app-id` + `github-app-key` for the owner of the request |
//! | `CFRG_BITBUCKET_TOKEN` | `bitbucket-token` (none exists in Frankfurt: Bitbucket destinations are skipped with one log line) |
use cfrg::{
    native::{
        env_name,
        http::{Http, SecretFn},
        Placement, Provider,
    },
    Result,
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

pub struct Creds {
    dir: Option<PathBuf>,
    /// Installation tokens by lowercase owner, kept for the run.
    minted: Rc<RefCell<BTreeMap<String, String>>>,
}

fn file_for(env: &str) -> Option<&'static str> {
    match env {
        "CFRG_FORGEJO_TOKEN" => Some("forgejo-token"),
        "CFRG_GITLAB_TOKEN" => Some("gitlab-token"),
        "CFRG_GITHUB_TOKEN" => Some("github-token"),
        "CFRG_BITBUCKET_TOKEN" => Some("bitbucket-token"),
        _ => None,
    }
}

fn read(dir: &Path, name: &str) -> Option<String> {
    let text = fs::read_to_string(dir.join(name)).ok()?;
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// The `owner` and the first repository of a GitHub API path (`/repos/o/r/...`) or of `owner/repo`; a path without a repository
/// (`/orgs/o/...`) gives `None`: the App's installation is resolved through a repository of the owner.
fn repo_of(path: &str) -> Option<(String, String)> {
    let path = path.trim_start_matches('/');
    if path.starts_with("orgs/") || path.starts_with("users/") {
        return None;
    }
    let path = path.strip_prefix("repos/").unwrap_or(path);
    let mut parts = path.split(['/', '?']);
    let (owner, repo) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !repo.is_empty())
        .then(|| (owner.to_owned(), repo.trim_end_matches(".git").to_owned()))
}

impl Clone for Creds {
    fn clone(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            minted: self.minted.clone(),
        }
    }
}

impl Creds {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            minted: Rc::default(),
        }
    }

    fn from_env(env: &str) -> Option<String> {
        std::env::var(env).ok().filter(|v| !v.is_empty())
    }

    /// Whether a credential for `env` exists at all (any GitHub owner for the App).
    pub fn available(&self, env: &str) -> bool {
        if Self::from_env(env).is_some() {
            return true;
        }
        let (Some(dir), Some(file)) = (&self.dir, file_for(env)) else {
            return false;
        };
        read(dir, file).is_some()
            || (env == "CFRG_GITHUB_TOKEN"
                && read(dir, "github-app-id").is_some()
                && read(dir, "github-app-key").is_some())
    }

    pub fn token(&self, env: &str, path: &str) -> Result<String> {
        env_name(env)?;
        if let (Some(dir), Some(file)) = (&self.dir, file_for(env)) {
            if let Some(t) = read(dir, file) {
                return Ok(t);
            }
            if env == "CFRG_GITHUB_TOKEN" {
                if let (Some(id), Some(key)) =
                    (read(dir, "github-app-id"), read(dir, "github-app-key"))
                {
                    return self.installation(&id, &key, path);
                }
            }
        }
        if let Some(t) = Self::from_env(env) {
            return Ok(t);
        }
        match env {
            "CFRG_BITBUCKET_TOKEN" => Err("no Bitbucket credential mounted".into()),
            _ => Err(format!("no credential for {env}: neither a file in the credentials directory nor the variable").into()),
        }
    }

    fn installation(&self, id: &str, key: &str, path: &str) -> Result<String> {
        let Some((owner, name)) = repo_of(path) else {
            // `/orgs/<owner>/...` carries no repository: it reuses the token already minted for the owner.
            let first = path.trim_start_matches('/');
            let first = ["orgs/", "users/"]
                .iter()
                .find_map(|p| first.strip_prefix(p))
                .unwrap_or(first);
            let owner = first
                .split(['/', '?'])
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            return self
                .minted
                .borrow()
                .get(&owner)
                .cloned()
                .ok_or_else(|| "a GitHub App token needs a repository of the owner first".into());
        };
        let owner = owner.to_ascii_lowercase();
        if let Some(t) = self.minted.borrow().get(&owner) {
            return Ok(t.clone());
        }
        let app_id: u64 = id.parse().map_err(|_| "github-app-id is not a number")?;
        let creds = cghb::relay::AppCredentials {
            app_id,
            private_key_pem: cghb::relay::Redacted::new(key.to_owned()),
        };
        let driver = cghb::relay::GithubRelay::new(&creds, "https://api.github.com")
            .map_err(|e| e.to_string())?;
        let repo =
            cghb::relay::Repo::parse(&format!("{owner}/{name}")).map_err(|e| e.to_string())?;
        let rt = cfrg_relay::live::runtime()?;
        let token = rt
            .block_on(driver.installation_token(&repo))
            .map_err(|e| format!("GitHub App token for {owner}: {e}"))?;
        let token = token.expose().to_owned();
        self.minted.borrow_mut().insert(owner, token.clone());
        Ok(token)
    }

    pub fn open(&self, state: &Path, apply: bool) -> Result<Http> {
        let me = self.clone();
        let resolve: SecretFn = Box::new(move |env, path| me.token(env, path));
        Ok(Http::open(state, apply)?.with_secrets(resolve))
    }

    /// The placement without the destinations whose credential is not mounted, each provider named once in the log.
    pub fn placement(&self, document: &[u8]) -> Result<Placement> {
        let mut placement = Placement::from_document(document)?;
        let mut skipped = BTreeMap::new();
        for repo in &mut placement.repositories {
            repo.destinations.retain(|d| {
                let ok = d.provider != Provider::Bitbucket || self.available(&d.endpoint.token_env);
                if !ok {
                    *skipped.entry("Bitbucket").or_insert(0u32) += 1;
                }
                ok
            });
        }
        for (name, n) in skipped {
            eprintln!("{name}: skipped ({n} destinations): no {name} credential mounted");
        }
        Ok(placement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (k, v) in files {
            fs::write(d.path().join(k), format!("{v}\n")).unwrap();
        }
        d
    }

    #[test]
    fn tokens_come_from_the_per_credential_files() {
        let d = dir(&[("forgejo-token", "f"), ("gitlab-token", "g")]);
        let c = Creds::new(Some(d.path().to_owned()));
        assert_eq!(c.token("CFRG_FORGEJO_TOKEN", "").unwrap(), "f");
        assert_eq!(c.token("CFRG_GITLAB_TOKEN", "/projects/1").unwrap(), "g");
        assert!(c.available("CFRG_GITLAB_TOKEN"));
    }

    #[test]
    fn a_missing_bitbucket_credential_says_so_and_drops_its_destinations() {
        let d = dir(&[("forgejo-token", "f")]);
        let c = Creds::new(Some(d.path().to_owned()));
        assert!(!c.available("CFRG_BITBUCKET_TOKEN"));
        assert_eq!(
            c.token("CFRG_BITBUCKET_TOKEN", "").unwrap_err().to_string(),
            "no Bitbucket credential mounted"
        );
        let doc = br#"{"schema":1,"source":{"origin":"https://f.example","token_env":"CFRG_FORGEJO_TOKEN"},"repositories":[
          {"path":"o/r","source_id":1,"private":false,"default_branch":"main","content":"native-git","destinations":[
           {"provider":"bitbucket","endpoint":{"origin":"https://api.bitbucket.org","token_env":"CFRG_BITBUCKET_TOKEN"},"namespace":"CORE","mirror_user":"u","password_env":"CFRG_BITBUCKET_TOKEN","interval_seconds":28800}]}]}"#;
        let p = c.placement(doc).unwrap();
        assert!(p.repositories.iter().all(|r| r.destinations.is_empty()));
    }

    #[test]
    fn the_github_owner_is_read_from_the_request_path() {
        assert_eq!(
            repo_of("/repos/Corbet-Labs/cfrg/branches").unwrap(),
            ("Corbet-Labs".into(), "cfrg".into())
        );
        assert_eq!(
            repo_of("corbet-nix/nix.git").unwrap(),
            ("corbet-nix".into(), "nix".into())
        );
        assert!(repo_of("/orgs/corbet-nix/repos").is_none());
    }

    #[test]
    fn without_any_credential_the_error_names_the_variable() {
        let d = dir(&[]);
        let c = Creds::new(Some(d.path().to_owned()));
        let e = c.token("CFRG_GITLAB_TOKEN", "").unwrap_err().to_string();
        assert!(e.contains("CFRG_GITLAB_TOKEN"));
    }
}
