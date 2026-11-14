//! The wiki push of the copies (operating model R40; Forge-router §5.9), as a library so that
//! `cfrg native --operation wiki` (all repositories) and `cfrg-serve` (one repository, when the
//! Forgejo `wiki` system hook arrives) run the same code. The decisions are [`super::wiki`].
//!
//! Declared GitLab destinations and PUBLIC GitHub destinations receive a push; private GitHub
//! and Bitbucket copies are counted as `link` and carry the link through the settings sync's
//! description ([`super::links`]). The GitHub leg pushes over HTTPS with the installation token
//! and honours the write gate's kill switch and pause folder. A
//! contributed project has no declared copies (R11), so it is never touched.
//!
//! Reads go over HTTPS with the API credentials of the placement, handed to git as an extra
//! header in the environment, never in argv or a file. The push goes over SSH with ONE wiki
//! deploy key (non-expiring, R31), which the apply enables with `can_push` on each GitLab copy it
//! pushes to. Its title is not `cfrg:remote_mirror_*`, so the mirror reconcile never removes it.
//! The push is a fast-forward or nothing, never forced; a diverged copy is held and reported.
use super::{
    encode,
    http::{expect, Auth, Request, Response, Transport},
    wiki::{self, Action, Copy, Leg},
    Destination, Placement, Provider, Repository,
};
use crate::{failure, process::Runner, Environment, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

/// The title the wiki deploy key carries on GitLab.
pub const KEY_TITLE: &str = "cfrg-wiki (R40)";

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Github => "github",
        Provider::Gitlab => "gitlab",
        Provider::Bitbucket => "bitbucket",
    }
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |a, (i, b)| a | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A git environment without ambient configuration: no system or global config, no prompt,
/// no ssh config, plus the given settings.
fn git_env(extra: &[(&str, String)]) -> Environment {
    let mut env = Environment::new();
    for key in ["PATH", "HOME", "LANG", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            env.insert(key.into(), value);
        }
    }
    env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
    env.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
    env.insert("GIT_ASKPASS".into(), "/bin/false".into());
    env.insert("SSH_ASKPASS".into(), "/bin/false".into());
    env.insert("GIT_SSH_COMMAND".into(), "/bin/false".into());
    for (k, v) in extra {
        env.insert(OsString::from(*k), OsString::from(v));
    }
    env
}

/// Git reads `origin` with this header (and nothing else is configured).
fn https_env(origin: &str, header: String) -> Environment {
    git_env(&[
        ("GIT_CONFIG_COUNT", "2".into()),
        ("GIT_CONFIG_KEY_0", format!("http.{origin}/.extraheader")),
        ("GIT_CONFIG_VALUE_0", header),
        ("GIT_CONFIG_KEY_1", "core.hooksPath".into()),
        ("GIT_CONFIG_VALUE_1", "/dev/null".into()),
    ])
}

fn plain_path(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .filter(|t| t.starts_with('/') && !t.chars().any(|c| c.is_whitespace() || c == '\''))
        .ok_or_else(|| {
            failure("Key and known_hosts paths must be absolute, without spaces or quotes")
        })?;
    Ok(text.to_string())
}

fn ssh_env(key: &Path, known_hosts: &Path) -> Result<Environment> {
    let command = format!(
        "ssh -F /dev/null -i {} -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile={}",
        plain_path(key)?,
        plain_path(known_hosts)?
    );
    let mut env = git_env(&[
        ("GIT_CONFIG_COUNT", "1".into()),
        ("GIT_CONFIG_KEY_0", "core.hooksPath".into()),
        ("GIT_CONFIG_VALUE_0", "/dev/null".into()),
    ]);
    env.insert("GIT_SSH_COMMAND".into(), command.into());
    Ok(env)
}

fn git(root: &Path, env: Environment, args: &[&str], secs: u64) -> Result<String> {
    let mut argv = vec!["git".to_string()];
    argv.extend(args.iter().map(|a| (*a).to_string()));
    Runner::new(root.to_path_buf(), env, Duration::from_secs(secs))?
        .with_stderr_events()
        .without_child_stderr()
        .run(&argv, true)
}

fn gitlab(
    io: &mut dyn Transport,
    dest: &Destination,
    method: &'static str,
    path: String,
    body: Option<Value>,
) -> Result<Response> {
    io.send(Request {
        endpoint: dest.endpoint.clone(),
        auth: Auth::PrivateToken,
        method,
        path: format!("/api/v4{path}"),
        body,
        scope: format!("{}/api", dest.endpoint.origin),
        creation: false,
    })
}

fn key_parts(text: &str) -> Vec<String> {
    text.split_whitespace().take(2).map(String::from).collect()
}

/// The id of the deploy key (matched by its type and key material, i.e. its fingerprint) from the instance-wide key list; best effort: the list
/// needs an administrator, so a refusal only leaves the id unknown (a project that already has the key teaches it through its own list).
fn lookup_key_id(
    io: &mut dyn Transport,
    dest: &Destination,
    public_key: &str,
    key_id: &mut Option<u64>,
) -> bool {
    for page in 1..=20 {
        let Ok(response) = gitlab(
            io,
            dest,
            "GET",
            format!("/deploy_keys?per_page=100&page={page}"),
            None,
        ) else {
            return false;
        };
        let Ok(list) = expect(response, &[200]) else {
            return false;
        };
        let Some(keys) = list.as_array() else {
            return false;
        };
        if let Some(id) = keys
            .iter()
            .find(|k| {
                k["key"]
                    .as_str()
                    .is_some_and(|t| key_parts(t) == key_parts(public_key))
            })
            .and_then(|k| k["id"].as_u64())
        {
            *key_id = Some(id);
            return true;
        }
        if keys.len() < 100 {
            break;
        }
    }
    false
}

/// Enable the wiki deploy key with `can_push` on one project. Idempotent: an enabled key
/// with push is left alone.
fn ensure_key(
    io: &mut dyn Transport,
    dest: &Destination,
    project: &str,
    public_key: &str,
    key_id: &mut Option<u64>,
) -> Result<&'static str> {
    let mut found = None;
    for page in 1..=20 {
        let list = expect(
            gitlab(
                io,
                dest,
                "GET",
                format!("/projects/{project}/deploy_keys?per_page=100&page={page}"),
                None,
            )?,
            &[200],
        )?;
        let keys = list.as_array().ok_or("Malformed deploy key list")?;
        for key in keys {
            if key["key"]
                .as_str()
                .is_some_and(|k| key_parts(k) == key_parts(public_key))
            {
                found = Some(key.clone());
                // The id of the key is the same on every project it is enabled on: remember it for the projects that lack it.
                if let Some(id) = key["id"].as_u64() {
                    key_id.get_or_insert(id);
                }
            }
        }
        if keys.len() < 100 {
            break;
        }
        if page == 20 {
            return Err("Deploy key pagination bound exceeded".into());
        }
    }
    let verify = |key: &Value| -> Result<()> {
        if key["can_push"] != true
            || key["key"]
                .as_str()
                .is_none_or(|k| key_parts(k) != key_parts(public_key))
        {
            return Err("Wiki deploy key capability or identity mismatch".into());
        }
        Ok(())
    };
    match found {
        Some(key) if key["can_push"] == true => Ok("key-present"),
        Some(key) => {
            let id = key["id"].as_u64().ok_or("Missing deploy key ID")?;
            let updated = expect(
                gitlab(
                    io,
                    dest,
                    "PUT",
                    format!("/projects/{project}/deploy_keys/{id}"),
                    Some(json!({"can_push": true})),
                )?,
                &[200],
            )?;
            verify(&updated)?;
            Ok("key-push-enabled")
        }
        // GitLab refuses to create a key that already exists on another project (400, fingerprint taken):
        // it is enabled by its id instead, and then given push on this project.
        None if key_id.is_some() || lookup_key_id(io, dest, public_key, key_id) => {
            let id = key_id.ok_or("No key id")?;
            expect(
                gitlab(
                    io,
                    dest,
                    "POST",
                    format!("/projects/{project}/deploy_keys/{id}/enable"),
                    None,
                )?,
                &[200, 201],
            )?;
            let updated = expect(
                gitlab(
                    io,
                    dest,
                    "PUT",
                    format!("/projects/{project}/deploy_keys/{id}"),
                    Some(json!({"can_push": true})),
                )?,
                &[200],
            )?;
            verify(&updated)?;
            Ok("key-enabled-by-id")
        }
        None => {
            let created = expect(
                gitlab(
                    io,
                    dest,
                    "POST",
                    format!("/projects/{project}/deploy_keys"),
                    Some(json!({"title": KEY_TITLE, "key": public_key, "can_push": true})),
                )?,
                &[201],
            )?;
            verify(&created)?;
            Ok("key-enabled")
        }
    }
}

/// The source wiki's objects, fetched once per repository into a private bare repository.
struct Source {
    dir: tempfile::TempDir,
    branch: String,
    head: String,
}

impl Source {
    fn fetch(url: &str, env: &Environment, branch: &str, head: &str) -> Result<Self> {
        let dir = tempfile::Builder::new().prefix("cfrg-wiki-").tempdir()?;
        git(
            dir.path(),
            env.clone(),
            &["init", "--quiet", "--bare", "."],
            60,
        )?;
        let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
        git(
            dir.path(),
            env.clone(),
            &["fetch", "--quiet", "--no-tags", "--", url, &refspec],
            300,
        )?;
        let fetched = git(
            dir.path(),
            env.clone(),
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{branch}^{{commit}}"),
            ],
            30,
        )?;
        if fetched.trim() != head {
            return Err(failure(
                "The source wiki moved while it was read; the next event retries",
            ));
        }
        Ok(Self {
            dir,
            branch: branch.into(),
            head: head.into(),
        })
    }

    /// Whether `commit` is an ancestor of the source head. A commit the source history does
    /// not contain is not (the copy carries something Forgejo does not have).
    fn ancestor(&self, commit: &str) -> Option<bool> {
        let env = git_env(&[]);
        if git(
            self.dir.path(),
            env.clone(),
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
            30,
        )
        .is_err()
        {
            return Some(false);
        }
        let base = git(
            self.dir.path(),
            env,
            &["merge-base", "--", commit, &self.head],
            30,
        )
        .ok()?;
        Some(base.trim() == commit)
    }
}

#[derive(Default)]
struct Count {
    copies: u32,
    in_sync: u32,
    seed: u32,
    fast_forward: u32,
    pushed: u32,
    absent: u32,
    held: BTreeMap<String, u32>,
    errors: u32,
}

impl Count {
    fn to_json(&self) -> Value {
        json!({"copies":self.copies, "in_sync":self.in_sync, "to_seed":self.seed,
            "to_fast_forward":self.fast_forward, "pushed":self.pushed, "absent":self.absent,
            "held":self.held, "errors":self.errors})
    }
}

/// Resolves the name of a credential (the placement's `token_env`) to its value. The command
/// line reads the environment; `cfrg-serve` reads its credentials directory.
/// Token by the endpoint's `token_env` name and the `owner/repo` it is used for (a GitHub App token is per owner).
pub type Secrets<'a> = &'a dyn Fn(&str, &str) -> Result<String>;

pub struct WikiOptions {
    pub key: Option<PathBuf>,
    pub known_hosts: Option<PathBuf>,
    pub max_pushes: usize,
    pub push_gap: Duration,
    /// Limit the run to one provider's copies.
    pub provider: Option<String>,
    pub write: bool,
    /// The id of the deploy key on GitLab, once it exists anywhere: copies enable it by id.
    pub key_id: Option<u64>,
    /// GitHub writes: the write gate's kill switch and pause folder. A GitHub push needs both.
    pub github_kill_switch: Option<PathBuf>,
    pub github_paused_dir: Option<PathBuf>,
}

/// One run over any number of repositories.
pub struct Session<'a> {
    options: WikiOptions,
    secrets: Secrets<'a>,
    source_origin: String,
    source_env: Environment,
    public_key: Option<String>,
    /// The id of the deploy key on GitLab: `--wiki-key-id` as an override, else learnt from GitLab during the run.
    key_id: Option<u64>,
    ssh: Option<Environment>,
    scratch: tempfile::TempDir,
    rows: Vec<Value>,
    push_counts: BTreeMap<String, Count>,
    link_counts: BTreeMap<String, u32>,
    pushes: usize,
    stopped: Option<String>,
    incomplete: bool,
}

/// What is read once per repository.
struct RepoState {
    head: Option<(String, String)>,
    fetched: Option<Source>,
    wiki_url: String,
}

impl<'a> Session<'a> {
    pub fn new(placement: &Placement, options: WikiOptions, secrets: Secrets<'a>) -> Result<Self> {
        let (public_key, ssh) = if options.write {
            let (Some(key), Some(known_hosts)) = (&options.key, &options.known_hosts) else {
                return Err("--apply needs --wiki-key and --wiki-known-hosts".into());
            };
            let public = std::fs::read_to_string(key.with_extension("pub"))
                .map_err(|_| failure("The wiki key's public half (<key>.pub) is missing"))?;
            if !public.starts_with("ssh-ed25519 ") {
                return Err("The wiki key must be an ssh-ed25519 key".into());
            }
            (
                Some(public.trim().to_string()),
                Some(ssh_env(key, known_hosts)?),
            )
        } else {
            (None, None)
        };
        let origin = placement.source.origin.trim_end_matches('/').to_string();
        let source_env = https_env(
            &origin,
            format!(
                "Authorization: token {}",
                secrets(&placement.source.token_env, "")?
            ),
        );
        Ok(Self {
            key_id: options.key_id,
            options,
            secrets,
            source_origin: origin,
            source_env,
            public_key,
            ssh,
            scratch: tempfile::Builder::new()
                .prefix("cfrg-wiki-run-")
                .tempdir()?,
            rows: Vec::new(),
            push_counts: BTreeMap::new(),
            link_counts: BTreeMap::new(),
            pushes: 0,
            stopped: None,
            incomplete: false,
        })
    }

    /// Bring the copies of one repository (which has a wiki on the primary) in line.
    pub fn repository(&mut self, io: &mut dyn Transport, source_path: &str, declared: &Repository) {
        let mut live = declared.clone();
        live.path = source_path.to_string();
        let mut state = RepoState {
            head: None,
            fetched: None,
            wiki_url: format!("{}/{}.wiki.git", self.source_origin, source_path),
        };
        for dest in &declared.destinations {
            let name = provider_name(dest.provider);
            if self
                .options
                .provider
                .as_deref()
                .is_some_and(|only| only != name)
            {
                continue;
            }
            let path = live.destination_path(dest);
            if wiki::leg(dest.provider, declared.private) == Leg::Link {
                *self.link_counts.entry(name.into()).or_default() += 1;
                continue;
            }
            let org = path.split('/').next().unwrap_or_default().to_string();
            let key = format!("{name}/{org}");
            self.push_counts.entry(key.clone()).or_default();
            if dest.absent {
                continue;
            }
            if let Some(why) = dest.hold.as_ref().or(declared.hold.as_ref()) {
                if let Some(c) = self.push_counts.get_mut(&key) {
                    *c.held.entry("declared-hold".into()).or_default() += 1;
                }
                self.rows.push(json!({"source":source_path, "provider":name, "destination":path, "state":"held", "reason":"declared-hold", "hold":why}));
                continue;
            }
            if let Some(why) = &self.stopped {
                self.incomplete = true;
                self.rows.push(json!({"source":source_path, "provider":name, "destination":path, "state":"provider-stopped", "reason":why}));
                continue;
            }
            let mut count = std::mem::take(self.push_counts.entry(key.clone()).or_default());
            let outcome = if dest.provider == Provider::Github {
                self.github_destination(io, &mut state, &mut count, dest, &path)
            } else {
                self.destination(io, &mut state, &mut count, dest, &path, declared.private)
            };
            self.push_counts.insert(key, count);
            let mut row = match outcome {
                Ok(row) => row,
                Err(error) => {
                    self.push_counts
                        .entry(format!("{name}/{org}"))
                        .or_default()
                        .errors += 1;
                    self.incomplete = true;
                    let text = error.to_string();
                    if ["HTTP 401", "HTTP 403", "HTTP 429", "HTTP 402", "hold for"]
                        .iter()
                        .any(|s| text.contains(s))
                    {
                        self.stopped = Some(text.clone());
                    }
                    json!({"state":"error", "error": text})
                }
            };
            row["source"] = json!(source_path);
            row["provider"] = json!(name);
            row["destination"] = json!(path);
            self.rows.push(row);
        }
    }

    fn destination(
        &mut self,
        io: &mut dyn Transport,
        state: &mut RepoState,
        count: &mut Count,
        dest: &Destination,
        path: &str,
        private: bool,
    ) -> Result<Value> {
        let project = encode(path);
        let found = gitlab(io, dest, "GET", format!("/projects/{project}"), None)?;
        if found.status == 404 {
            count.absent += 1;
            return Ok(json!({"state":"copy-absent"}));
        }
        let project_value = expect(found, &[200])?;
        let wanted = if private { "private" } else { "public" };
        if project_value["path_with_namespace"]
            .as_str()
            .is_none_or(|p| !p.eq_ignore_ascii_case(path))
            || project_value["visibility"] != wanted
        {
            return Err("GitLab copy identity or visibility differs from the primary".into());
        }
        count.copies += 1;
        if project_value["wiki_access_level"] != "enabled" {
            *count.held.entry("copy-wiki-off".into()).or_default() += 1;
            return Ok(
                json!({"state":"held", "reason":"copy-wiki-off", "note":"the settings sync (R42) switches it on"}),
            );
        }
        if state.head.is_none() {
            let text = git(
                self.scratch.path(),
                self.source_env.clone(),
                &["ls-remote", "--symref", "--", &state.wiki_url, "HEAD"],
                60,
            )?;
            state.head =
                Some(wiki::read_source_head(&text).ok_or("The Forgejo wiki has no readable HEAD")?);
        }
        let (branch, source_head) = state.head.clone().ok_or("No source head")?;
        let origin = dest.endpoint.origin.trim_end_matches('/');
        let copy_url = format!("{origin}/{path}.wiki.git");
        let copy_env = || -> Result<Environment> {
            Ok(https_env(
                origin,
                format!(
                    "Authorization: Basic {}",
                    base64(
                        format!("oauth2:{}", (self.secrets)(&dest.endpoint.token_env, path)?)
                            .as_bytes()
                    )
                ),
            ))
        };
        let text = git(
            self.scratch.path(),
            copy_env()?,
            &["ls-remote", "--", &copy_url, "refs/heads/*"],
            60,
        )?;
        let copy = wiki::read_copy(&text, &branch);
        let ancestor = match &copy {
            Copy::At(at) if *at != source_head => {
                if state.fetched.is_none() {
                    state.fetched = Some(Source::fetch(
                        &state.wiki_url,
                        &self.source_env,
                        &branch,
                        &source_head,
                    )?);
                }
                state.fetched.as_ref().and_then(|s| s.ancestor(at))
            }
            _ => None,
        };
        let action = wiki::decide(&source_head, &copy, ancestor);
        let copy_head = match &copy {
            Copy::At(at) => Some(at.clone()),
            _ => None,
        };
        let mut row = json!({"state": action, "branch": branch, "source_head": source_head, "copy_head": copy_head});
        match action {
            Action::InSync => count.in_sync += 1,
            Action::Seed => count.seed += 1,
            Action::FastForward => count.fast_forward += 1,
            Action::Held(reason) => {
                *count.held.entry(reason.into()).or_default() += 1;
                row["state"] = json!("held");
                row["reason"] = json!(reason);
            }
        }
        if !(self.options.write && action.writes()) {
            return Ok(row);
        }
        if self.pushes >= self.options.max_pushes {
            row["pushed"] = json!(false);
            row["note"] = json!("--wiki-max-pushes reached; the next run continues");
            return Ok(row);
        }
        if state.fetched.is_none() {
            state.fetched = Some(Source::fetch(
                &state.wiki_url,
                &self.source_env,
                &branch,
                &source_head,
            )?);
        }
        let src = state.fetched.as_ref().ok_or("No fetched source")?;
        let key = ensure_key(
            io,
            dest,
            &project,
            self.public_key.as_deref().ok_or("No wiki key")?,
            &mut self.key_id,
        )?;
        if self.pushes > 0 {
            std::thread::sleep(self.options.push_gap);
        }
        self.pushes += 1;
        let host = origin.trim_start_matches("https://");
        let ssh_url = format!("ssh://git@{host}/{path}.wiki.git");
        let target = format!("{}:refs/heads/{}", src.head, src.branch);
        git(
            src.dir.path(),
            self.ssh.clone().ok_or("No ssh environment")?,
            &["push", "--quiet", "--no-verify", "--", &ssh_url, &target],
            300,
        )?;
        let after = git(
            self.scratch.path(),
            copy_env()?,
            &[
                "ls-remote",
                "--",
                &copy_url,
                &format!("refs/heads/{}", src.branch),
            ],
            60,
        )?;
        if wiki::read_copy(&after, &src.branch) != Copy::At(src.head.clone()) {
            return Err("The copy wiki does not show the pushed commit".into());
        }
        count.pushed += 1;
        row["pushed"] = json!(true);
        row["key"] = json!(key);
        Ok(row)
    }

    /// A public GitHub copy: the same decision as GitLab, but the copy is read through the REST
    /// API, and read and push go over HTTPS with the installation token (no deploy key).
    /// Every write waits for the gate's files: a kill switch or a paused organisation holds it.
    fn github_destination(
        &mut self,
        io: &mut dyn Transport,
        state: &mut RepoState,
        count: &mut Count,
        dest: &Destination,
        path: &str,
    ) -> Result<Value> {
        let found = io.send(Request {
            endpoint: dest.endpoint.clone(),
            auth: Auth::Bearer,
            method: "GET",
            path: format!("/repos/{path}"),
            body: None,
            scope: format!("{}/repos", dest.endpoint.origin),
            creation: false,
        })?;
        if found.status == 404 {
            count.absent += 1;
            return Ok(json!({"state":"copy-absent"}));
        }
        let repo = expect(found, &[200])?;
        if repo["full_name"]
            .as_str()
            .is_none_or(|p| !p.eq_ignore_ascii_case(path))
            || repo["private"] != false
        {
            return Err("GitHub copy identity or visibility differs from the primary".into());
        }
        count.copies += 1;
        let origin = "https://github.com";
        let copy_url = format!("{origin}/{path}.wiki.git");
        let copy_env = || -> Result<Environment> {
            Ok(https_env(
                origin,
                format!(
                    "Authorization: Basic {}",
                    base64(
                        format!(
                            "x-access-token:{}",
                            (self.secrets)(&dest.endpoint.token_env, path)?
                        )
                        .as_bytes()
                    )
                ),
            ))
        };
        let listed = git(
            self.scratch.path(),
            copy_env()?,
            &["ls-remote", "--", &copy_url, "refs/heads/*"],
            60,
        );
        let text = match listed {
            Ok(text) => text,
            Err(error) => {
                // GitHub creates `<repo>.wiki.git` only after a first page exists, a web-UI act nobody
                // scripts (R9). The copy is then treated like a private one (wiki off, description link):
                // no hold, and the first run that finds the repository switches to the copy.
                let why = error.to_string();
                if why.contains("not found") || why.contains("Repository not found") {
                    *self.link_counts.entry("github".into()).or_default() += 1;
                    return Ok(json!({"state":"link", "reason":"wiki-repository-not-initialised"}));
                }
                return Err(error);
            }
        };
        if repo["has_wiki"] != true {
            // The repository exists but the wiki is off: the settings sync switches it on first.
            return Ok(json!({"state":"waiting-for-settings", "reason":"copy-wiki-off"}));
        }
        if state.head.is_none() {
            let text = git(
                self.scratch.path(),
                self.source_env.clone(),
                &["ls-remote", "--symref", "--", &state.wiki_url, "HEAD"],
                60,
            )?;
            state.head =
                Some(wiki::read_source_head(&text).ok_or("The Forgejo wiki has no readable HEAD")?);
        }
        let (branch, source_head) = state.head.clone().ok_or("No source head")?;
        let copy = wiki::read_copy(&text, &branch);
        let ancestor = match &copy {
            Copy::At(at) if *at != source_head => {
                if state.fetched.is_none() {
                    state.fetched = Some(Source::fetch(
                        &state.wiki_url,
                        &self.source_env,
                        &branch,
                        &source_head,
                    )?);
                }
                state.fetched.as_ref().and_then(|s| s.ancestor(at))
            }
            _ => None,
        };
        let action = wiki::decide(&source_head, &copy, ancestor);
        let copy_head = match &copy {
            Copy::At(at) => Some(at.clone()),
            _ => None,
        };
        let mut row = json!({"state": action, "branch": branch, "source_head": source_head, "copy_head": copy_head});
        match action {
            Action::InSync => count.in_sync += 1,
            Action::Seed => count.seed += 1,
            Action::FastForward => count.fast_forward += 1,
            Action::Held(reason) => {
                *count.held.entry(reason.into()).or_default() += 1;
                row["state"] = json!("held");
                row["reason"] = json!(reason);
            }
        }
        if !(self.options.write && action.writes()) {
            return Ok(row);
        }
        let org = path.split('/').next().unwrap_or_default();
        let (Some(kill), Some(paused)) = (
            &self.options.github_kill_switch,
            &self.options.github_paused_dir,
        ) else {
            return Err(
                "a GitHub wiki push needs the write gate's kill switch and pause folder".into(),
            );
        };
        if kill.exists() || paused.join(org).exists() {
            row["pushed"] = json!(false);
            row["note"] = json!("held by the write gate (kill switch or paused organisation)");
            return Ok(row);
        }
        if self.pushes >= self.options.max_pushes {
            row["pushed"] = json!(false);
            row["note"] = json!("--wiki-max-pushes reached; the next run continues");
            return Ok(row);
        }
        if state.fetched.is_none() {
            state.fetched = Some(Source::fetch(
                &state.wiki_url,
                &self.source_env,
                &branch,
                &source_head,
            )?);
        }
        let src = state.fetched.as_ref().ok_or("No fetched source")?;
        if self.pushes > 0 {
            std::thread::sleep(self.options.push_gap);
        }
        self.pushes += 1;
        let target = format!("{}:refs/heads/{}", src.head, src.branch);
        git(
            src.dir.path(),
            copy_env()?,
            &["push", "--quiet", "--no-verify", "--", &copy_url, &target],
            300,
        )?;
        let after = git(
            self.scratch.path(),
            copy_env()?,
            &[
                "ls-remote",
                "--",
                &copy_url,
                &format!("refs/heads/{}", src.branch),
            ],
            60,
        )?;
        if wiki::read_copy(&after, &src.branch) != Copy::At(src.head.clone()) {
            return Err("The copy wiki does not show the pushed commit".into());
        }
        count.pushed += 1;
        row["pushed"] = json!(true);
        Ok(row)
    }

    /// True when some copy was not brought in line (an error or a stopped provider).
    pub fn incomplete(&self) -> bool {
        self.incomplete
    }

    /// Held copies (diverged or otherwise), for the caller to report.
    pub fn held(&self) -> usize {
        self.rows.iter().filter(|r| r["state"] == "held").count()
    }

    pub fn report(&self) -> Value {
        let counts: BTreeMap<String, Value> = self
            .push_counts
            .iter()
            .map(|(k, c)| (k.clone(), c.to_json()))
            .collect();
        json!({
            "operation": "wiki",
            "apply": self.options.write,
            "complete": !self.incomplete,
            "push": counts,
            "link": self.link_counts,
            "pushes": self.pushes,
            "rows": self.rows,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b"oauth2:t"), "b2F1dGgyOnQ=");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
    }

    #[test]
    fn git_runs_without_ambient_configuration_and_never_prompts() {
        let env = git_env(&[]);
        assert_eq!(
            env.get(&OsString::from("GIT_CONFIG_NOSYSTEM")),
            Some(&"1".into())
        );
        assert_eq!(
            env.get(&OsString::from("GIT_CONFIG_GLOBAL")),
            Some(&"/dev/null".into())
        );
        assert_eq!(
            env.get(&OsString::from("GIT_TERMINAL_PROMPT")),
            Some(&"0".into())
        );
        assert!(!env.contains_key(&OsString::from("SSH_AUTH_SOCK")));
    }

    #[test]
    fn the_credential_travels_in_the_environment_for_its_origin_only() {
        let env = https_env("https://gitlab.com", "Authorization: Basic X".into());
        assert_eq!(
            env.get(&OsString::from("GIT_CONFIG_KEY_0")),
            Some(&"http.https://gitlab.com/.extraheader".into())
        );
    }

    #[test]
    fn ssh_refuses_paths_that_would_split_the_command() {
        assert!(ssh_env(Path::new("/k ey"), Path::new("/kh")).is_err());
        assert!(ssh_env(Path::new("relative"), Path::new("/kh")).is_err());
        let env = ssh_env(Path::new("/run/k"), Path::new("/run/kh")).unwrap();
        let command = env.get(&OsString::from("GIT_SSH_COMMAND")).unwrap();
        assert!(command
            .to_string_lossy()
            .contains("StrictHostKeyChecking=yes"));
        assert!(command.to_string_lossy().contains("-F /dev/null"));
    }

    struct Gitlab {
        project_keys: Vec<Value>,
        all_keys: Option<Vec<Value>>,
        seen: Vec<String>,
    }
    impl Transport for Gitlab {
        fn send(&mut self, request: Request) -> Result<Response> {
            self.seen.push(request.path.clone());
            let body = if request.path.starts_with("/api/v4/deploy_keys") {
                match &self.all_keys {
                    Some(k) => json!(k),
                    None => {
                        return Ok(Response {
                            status: 403,
                            body: json!({}),
                        })
                    }
                }
            } else {
                json!(self.project_keys)
            };
            Ok(Response { status: 200, body })
        }
    }
    fn gitlab_dest() -> Destination {
        serde_json::from_value(json!({"provider":"gitlab","endpoint":{"origin":"https://gitlab.example","token_env":"T"},"path":"g/r","namespace":"2","repository_id":"7","mirror_user":"9","password_env":"T","interval_seconds":600})).unwrap()
    }
    const PUB: &str = "ssh-ed25519 AAAAkey wiki@bridge";

    #[test]
    fn the_key_id_is_learnt_from_the_instance_list_by_fingerprint_when_the_project_lacks_the_key() {
        let mut io = Gitlab {
            project_keys: vec![],
            all_keys: Some(vec![
                json!({"id": 5, "key": "ssh-ed25519 OTHER x"}),
                json!({"id": 42, "key": "ssh-ed25519 AAAAkey comment"}),
            ]),
            seen: vec![],
        };
        let mut id = None;
        assert!(lookup_key_id(&mut io, &gitlab_dest(), PUB, &mut id));
        assert_eq!(id, Some(42));
    }

    #[test]
    fn a_refused_instance_list_leaves_the_id_unknown_and_a_project_that_has_the_key_teaches_it() {
        let mut io = Gitlab {
            project_keys: vec![],
            all_keys: None,
            seen: vec![],
        };
        let mut id = None;
        assert!(!lookup_key_id(&mut io, &gitlab_dest(), PUB, &mut id));
        assert_eq!(id, None);
        io.project_keys = vec![json!({"id": 9, "key": PUB, "can_push": true})];
        assert_eq!(
            ensure_key(&mut io, &gitlab_dest(), "7", PUB, &mut id).unwrap(),
            "key-present"
        );
        assert_eq!(id, Some(9), "the id found on one project serves the others");
    }
}
