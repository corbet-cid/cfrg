//! Configuration (`config.json`) and credentials (one file per key in a directory, like a mounted Secret).
//!
//! The file is the one the Python bridge read, with the receiver's own additions: the dispatch target and the
//! placement document that declares the contributed projects.
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub listeners: BTreeMap<String, ListenerConfig>,
    /// CA bundle of the cluster: the ONLY trust anchor of the Forgejo client (the providers keep the public roots).
    pub forge_ca: Option<String>,
    pub forge_api: String,
    pub forge_web: String,
    pub github: GithubConfig,
    pub gitlab: GitlabConfig,
    /// provider -> primary path -> copy path, for the few renamed copies.
    #[serde(default)]
    pub aliases: BTreeMap<String, BTreeMap<String, String>>,
    /// The placement document that declares the contributed projects (R10, R11). Absent = none are declared.
    pub placement: Option<String>,
    /// The public spellings of the primary forge (the first is the web host of the primary).
    #[serde(default = "default_forge_hosts")]
    pub forge_hosts: Vec<String>,
    pub dispatch: Option<DispatchConfig>,
    /// Run the release upload as this external command (argv; `OWNER/REPO TAG` are appended) instead of inside the
    /// service, for example a locked unit elsewhere. Absent = inline. The service then holds no provider key.
    pub release_command: Option<Vec<String>>,
    #[serde(default)]
    pub replay: ReplayConfig,
}

fn default_forge_hosts() -> Vec<String> {
    vec!["forge.corbet.ch".into(), "git.corbet.ch".into()]
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default)]
    pub port: u16,
    /// Directory with `tls.crt` and `tls.key`: the listener speaks TLS 1.3 only.
    pub tls_dir: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn default_listen() -> String {
    "0.0.0.0".into()
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    pub api: String,
    pub uploads: String,
    pub web: Option<String>,
    #[serde(default = "three")]
    pub min_write_interval_s: f64,
    pub orgs: Vec<String>,
}

fn three() -> f64 {
    3.0
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitlabConfig {
    pub api: String,
    pub groups: Vec<String>,
}

/// Where the receiver starts the relay job: the infra repository's workflow, on the Forgejo of `forge_api`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchConfig {
    /// `owner/repo` of the repository that holds `relay.yml`.
    pub repository: String,
    #[serde(default = "relay_yml")]
    pub workflow: String,
    #[serde(default = "main_ref", rename = "ref")]
    pub git_ref: String,
}

fn relay_yml() -> String {
    "relay.yml".into()
}

fn main_ref() -> String {
    "main".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayConfig {
    /// How long a delivery id is remembered, in seconds.
    #[serde(default = "a_day")]
    pub ttl_seconds: u64,
    #[serde(default = "ten_thousand")]
    pub capacity: usize,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            ttl_seconds: a_day(),
            capacity: ten_thousand(),
        }
    }
}

fn a_day() -> u64 {
    86_400
}

fn ten_thousand() -> usize {
    10_000
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    pub fn copy_path(&self, provider: &str, repo: &str) -> String {
        self.aliases
            .get(provider)
            .and_then(|map| map.get(repo))
            .cloned()
            .unwrap_or_else(|| repo.to_owned())
    }
}

/// The credentials, by key name. One HMAC key per forge: a key that leaks from one provider signs nothing for another.
#[derive(Debug, Clone, Default)]
pub struct Credentials(BTreeMap<String, String>);

pub const CREDENTIAL_KEYS: [&str; 9] = [
    "forge_token",
    "github_token",
    "gitlab_token",
    "dispatch_token",
    "hmac_forgejo",
    "hmac_github",
    "hmac_gitlab",
    "hmac_bitbucket",
    "hmac_review",
];

impl Credentials {
    pub fn load(directory: &Path) -> Self {
        let mut map = BTreeMap::new();
        for key in CREDENTIAL_KEYS {
            if let Ok(text) = fs::read_to_string(directory.join(key)) {
                let value = text.trim().to_owned();
                if !value.is_empty() {
                    map.insert(key.to_owned(), value);
                }
            }
        }
        Self(map)
    }

    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}
