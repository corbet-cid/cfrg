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
    time::{Duration, Instant},
};

/// A GitHub installation token lives one hour; the run itself lasts hours (the daily takes 2 to 4 h), so a token minted at the start
/// is dead long before the end (HTTP 401 stops the whole provider). It is re-minted after this age.
const MINTED_MAX_AGE: Duration = Duration::from_secs(45 * 60);

#[derive(Clone)]
struct Minted {
    token: String,
    /// The repository (`name`) the token was minted for: an `/orgs/<owner>/...` request has none, so a stale token is renewed with it.
    repo: String,
    at: Instant,
}

impl Minted {
    fn fresh(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.at) < MINTED_MAX_AGE
    }
}

pub struct Creds {
    dir: Option<PathBuf>,
    /// Installation tokens by lowercase owner, renewed when older than [`MINTED_MAX_AGE`].
    minted: Rc<RefCell<BTreeMap<String, Minted>>>,
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
        // Creating and reading a push mirror needs `write:repository`, which the relay's `forgejo-token` (issues only) must not have:
        // those requests use `forgejo-mirror-token` when the directory holds one.
        if let (Some(dir), "CFRG_FORGEJO_TOKEN", true) =
            (&self.dir, env, path.contains("/push_mirrors"))
        {
            if let Some(t) = read(dir, "forgejo-mirror-token") {
                return Ok(t);
            }
        }
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
        let (owner, name) = match repo_of(path) {
            Some((owner, name)) => (owner.to_ascii_lowercase(), name),
            None => {
                // `/orgs/<owner>/...` carries no repository: it reuses (and, when stale, renews with the repository it was minted for)
                // the token already minted for the owner.
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
                let known = self.minted.borrow().get(&owner).cloned();
                match known {
                    Some(m) if m.fresh(Instant::now()) => return Ok(m.token),
                    Some(m) => (owner, m.repo),
                    None => {
                        return Err(
                            "a GitHub App token needs a repository of the owner first".into()
                        )
                    }
                }
            }
        };
        if let Some(m) = self.minted.borrow().get(&owner) {
            if m.fresh(Instant::now()) {
                return Ok(m.token.clone());
            }
        }
        let rt = cfrg_relay::live::runtime()?;
        let driver = app_driver(&rt, id, key)?;
        let repo =
            cghb::relay::Repo::parse(&format!("{owner}/{name}")).map_err(|e| e.to_string())?;
        let token = rt
            .block_on(driver.installation_token(&repo))
            .map_err(|e| format!("GitHub App token for {owner}: {e}"))?;
        let token = token.expose().to_owned();
        self.minted.borrow_mut().insert(
            owner,
            Minted {
                token: token.clone(),
                repo: name,
                at: Instant::now(),
            },
        );
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
        self.credentialed(&mut placement, 0);
        Ok(placement)
    }

    /// Drops the destinations whose credential is not mounted from the repositories at `from` and after (the derived
    /// rows are appended behind the declared ones and pass through the same rule).
    pub fn credentialed(&self, placement: &mut Placement, from: usize) {
        let mut skipped = BTreeMap::new();
        for repo in placement.repositories.iter_mut().skip(from) {
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
    }
}

/// The GitHub App client. octocrab's tower buffer spawns a task when the client is built, so it is built with the
/// runtime entered (built outside one it panics: "there is no reactor running").
fn app_driver(
    rt: &std::rc::Rc<cfrg_relay::live::Runtime>,
    id: &str,
    key: &str,
) -> Result<cghb::relay::GithubRelay> {
    let app_id: u64 = id.parse().map_err(|_| "github-app-id is not a number")?;
    let creds = cghb::relay::AppCredentials {
        app_id,
        private_key_pem: cghb::relay::Redacted::new(key.to_owned()),
    };
    let _entered = rt.enter();
    Ok(
        cghb::relay::GithubRelay::new(&creds, "https://api.github.com")
            .map_err(|e| e.to_string())?,
    )
}

#[cfg(test)]
mod minted_tests {
    use super::*;

    #[test]
    fn a_minted_token_is_renewed_after_its_max_age() {
        let at = Instant::now();
        let m = Minted {
            token: "t".into(),
            repo: "r".into(),
            at,
        };
        assert!(m.fresh(at));
        assert!(m.fresh(at + MINTED_MAX_AGE - Duration::from_secs(1)));
        assert!(!m.fresh(at + MINTED_MAX_AGE));
        assert!(MINTED_MAX_AGE < Duration::from_secs(3600));
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
    fn push_mirror_requests_use_the_mirror_token_when_there_is_one() {
        let d = dir(&[("forgejo-token", "f"), ("forgejo-mirror-token", "m")]);
        let c = Creds::new(Some(d.path().to_owned()));
        assert_eq!(
            c.token("CFRG_FORGEJO_TOKEN", "/repos/o/r/push_mirrors")
                .unwrap(),
            "m"
        );
        assert_eq!(c.token("CFRG_FORGEJO_TOKEN", "/repos/o/r").unwrap(), "f");
        let without = dir(&[("forgejo-token", "f")]);
        let c = Creds::new(Some(without.path().to_owned()));
        assert_eq!(
            c.token("CFRG_FORGEJO_TOKEN", "/repos/o/r/push_mirrors")
                .unwrap(),
            "f"
        );
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

    #[test]
    fn the_github_app_client_is_built_inside_the_runtime() {
        let rt = cfrg_relay::live::runtime().unwrap();
        app_driver(&rt, "1", TEST_KEY).expect("the client builds without a ambient runtime");
        assert!(app_driver(&rt, "x", TEST_KEY).is_err());
    }

    /// A throwaway key made for this test.
    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDANkf7nr8u3CoA
ta2unUZe3C0SDjUwZ8FnP2Vv23YOe8jjecqsUSJC5o9Ibcm0izM/SOR94BF+TQ7I
Vwnad1c/bfY3t4vkjut/vgYItDhRPlKo5Su2KfkM1Fr1U7czosdPcavwjZLT1kRO
wgJipKG1tdSQ9yR+NnTfztSMLwGaQjaCyWwuCaUwBkyoMnstUZ4qMWm04CQq0Z/q
RtHnxxM9NW9+kPq8Oj7n9vzji3w58MhsmYxg4YX5iZUAiddcUemCgRtIAqWS7LA/
CMLWRdHOletg81UI55oQtIM6fV3oGjTAiw2bglB5ZCoGEsGZeOIOo1FkE8eoaX3A
8TV65cH3AgMBAAECggEAMWAAP7e4vs3ZkFMbKYbHI+kFPuUdbrDIcsBLS1+hxPix
UxXfJUK2PV9CoUVqkhFL+c68u0UK2VnsGTSamF8dr3KM1CU3L2AzSJ5EqIm7Ccur
qi0sfF/t1Qjb0LdPkVkYP+UgWOvVMsuQwnZ97F8EF0x9LTv/SNiuTyuvV89fZwN3
N8qOqdTAp3PYDRALG3AHVtO0dD0t1vlVpzMS6DKY6miISPwNxQdTvokgGMuycRi0
hSMbiDTZFN1WtnekqA+hmYCHAXqttH/9FPN0DBxKVQBFenQVj+qH7KoSHk/IcX8/
qbl1om0DsUoHi2Zt9tOasYPf14xhu9sEFD+4vBJ+YQKBgQDmQXxqz4asw9RYp2rm
Uiqte6l1Gza3/rFNNPLf9n1M8WCLIivgTlejHYlq5/MHPk8ix++fKiuhQIWgBHq9
jFvvQiw0StU/KyN6wT69Zn7cCJFSAF7crsgd2wjRaSzTANGji4zvO64QxtAwdJGq
MA2Zj0daCTsMS/JqfiDi7toEsQKBgQDVs+JydBFmyaVsSYaSS0g6+4U+lI7bEgeG
7yl8tk9ea83LmNXVBqHVKqf70hXMweu9EEE8SESlmvIsAR4IGfNtkyDvK4A5//3c
I67Y7yAzJGUfRiCgnl4y6Wkb6C/C46N741UUJGR+vR7L+Rc76yLob89FY0R+1gdu
RHGh4c57JwKBgQDedpDVxbGxw/aqvP5Mj0CKorrj2Sox9RKqrKin+EexilijtJ09
t5N2lngJYRbufaYJzbBXUHAc+PDh3NIVxsGgVYfN8is8P9qkpIaQibGxt1loJ9F/
WCMirVhEs2BKcTUa1R6SmU+hB+Vnm4iNLKZHIiZm+Boj1FDf0QLHHsxn0QKBgQCj
JOELX3Cpd+fB85+4r6BE5x+h1uf4dpsoeAzEGKoe+03w0IzuRFHn2tjDdm8OEj1V
6MLksoc1pkSbM38p0CX20QujiyIpJTU4ikVBd8ytPmu0pGTkOgLP+0Ey/N75ea+l
NLgO2HmgB+Eq9s61/5x2tmUWI+J2sg6aL8ZQP4LSoQKBgA4+v8bENG9mD8DwzZ8S
zx3kvQMhvVcNG1UsVHFM8+cz7ZLL+DJNI8NhRzMimRzK4cCG5JC6ECU7+DoJqkEA
GznTp7eNx7tS2xvNkCx4h/CL8wgos/oqNRguT3NOIhcKbW5RMjmXaPL7JNsyJgEh
6wTJw2JnLNN/+RdkVeWmxi9u
-----END PRIVATE KEY-----";
}
