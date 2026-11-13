//! Configuration (`config.json`) and credentials (one file per key in a directory, like a mounted Secret).
//!
//! The file is the one the Python bridge read, with the receiver's own addition: the placement document that declares the contributed projects.
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
    /// The wiki leg (R40). Absent = a wiki hook is refused.
    pub wiki: Option<WikiConfig>,
    /// Event hooks (src/hooks): command key (`copy`, `downsync`, `lockrefresh`) -> argv; the job's arguments are appended.
    /// A hook whose key is absent answers 503.
    #[serde(default)]
    pub hook_commands: BTreeMap<String, Vec<String>>,
    /// The in-process relay (cfrg-relay): its configuration, the placement and the credentials directory.
    pub relay: Option<RelayConfig>,
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

/// The relay of `cfrg-relay`: files of the image/ConfigMap and the credentials directory (a Kubernetes Secret volume).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayConfig {
    /// The relay configuration (identities, limits); no repository list.
    pub config: String,
    /// The placement document the repository set is derived from.
    pub placement: String,
    /// Holds the database files (`host`, `port`, `dbname`, `username`, `password`) and the provider credentials.
    #[serde(default = "credentials")]
    pub credentials_dir: String,
    /// CA file the database connection is verified against (`sslmode=verify-full`).
    pub database_ca: Option<String>,
}

fn credentials() -> String {
    "/credentials".into()
}

/// Where the wiki leg keeps its state and finds its deploy key (the key's private half is a mounted Secret file,
/// its public half sits beside it as `<key>.pub`). The API tokens come from the credentials directory, by the
/// placement's `token_env` names.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiConfig {
    pub state: String,
    pub key: String,
    pub known_hosts: String,
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

/// (file name, key): the HMAC keys keep their names; the provider files are the names of `docs/relay.md`.
pub const CREDENTIAL_KEYS: [(&str, &str); 7] = [
    ("hmac_forgejo", "hmac_forgejo"),
    ("hmac_github", "hmac_github"),
    ("hmac_gitlab", "hmac_gitlab"),
    ("forgejo-token", "forge_token"),
    ("gitlab-token", "gitlab_token"),
    ("github-token", "github_token"),
    ("github-app-key", "github_app_key"),
];

impl Credentials {
    pub fn load(directory: &Path) -> Self {
        let mut map = BTreeMap::new();
        for (file, key) in CREDENTIAL_KEYS {
            if let Ok(text) = fs::read_to_string(directory.join(file)) {
                let value = text.trim().to_owned();
                if !value.is_empty() {
                    map.insert(key.to_owned(), value);
                }
            }
        }
        Self(map)
    }

    /// Add what `directory` holds (keys already present stay).
    pub fn merge(mut self, directory: &Path) -> Self {
        for (key, value) in Self::load(directory).0 {
            self.0.entry(key).or_insert(value);
        }
        self
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
