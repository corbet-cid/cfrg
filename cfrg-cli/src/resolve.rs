//! `cfrg resolve`: run bounded store probes for a validated clmr request and
//! emit the JSON response or rendered git config. Plus `cfrg
//! credential-helper`, the git-credential `get` responder reading tokens
//! from per-job env only.
use cfrg::{process::Runner, status::StatusTarget, Environment, Result};
use clmr::{Outcome, PrimarySource, Prober, RefKind, Request, Store};
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

/// Config keys that must never appear in a probed filesystem repository.
/// Lowercase substring match over a size-capped config read.
const FORBIDDEN_CONFIG_KEYS: &[&str] = &[
    "hookspath",
    "fsmonitor",
    "sshcommand",
    "askpass",
    "include",
    "alternates",
    "partialclone",
    "promisor",
];

pub fn run_resolve(request_path: PathBuf, emit: &str) -> Result<()> {
    if emit != "json" && emit != "gitconfig" {
        return Err("resolve --emit accepts json or gitconfig".into());
    }
    let mut request: Request = serde_json::from_slice(&std::fs::read(request_path)?)?;
    request.validate()?;
    let root = std::env::current_dir()?;
    enrich_primaries(&mut request, &root);
    let prober = CliProber {
        stores: request.stores.clone(),
        timeout_secs: request.timeout_secs,
        root,
    };
    let response = clmr::select(&request, &prober)?;
    match emit {
        "json" => println!("{}", serde_json::to_string_pretty(&response)?),
        _ => print!("{}", response.git_config),
    }
    Ok(())
}

/// Fill unknown primaries for moving refs from the declared redirect
/// authority (explicit request primaries win; pinned refs skip lookup).
/// Every lookup failure leaves null (canonical fallback), never guessed.
fn enrich_primaries(request: &mut Request, root: &Path) {
    let Some(source) = request.primary_source.clone() else {
        return;
    };
    // Read the declared file once. Never fall back to a network lookup when
    // a declared file is missing or malformed: unknown stays on the pointer.
    let placement = source.placement_file.as_ref().and_then(|path| {
        let file = std::fs::File::open(path).ok()?;
        let mut bytes = Vec::new();
        file.take(1_048_577).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 1_048_576 {
            return None;
        }
        let placement: clmr::Placement = serde_json::from_slice(&bytes).ok()?;
        placement.validate().ok()?;
        Some(placement)
    });
    for repo in &mut request.repositories {
        if repo.primary.is_some() {
            continue;
        }
        if !matches!(repo.r#ref, RefKind::Moving(_)) {
            continue;
        }
        if source.placement_file.is_some() {
            repo.primary = placement
                .as_ref()
                .and_then(|p| source.identities.get(p.primary(&repo.path)).cloned());
            continue;
        }
        let bound = source.timeout_secs.min(request.timeout_secs).max(1);
        if let Some(identity) = lookup_primary(&source, &repo.path, bound, root) {
            repo.primary = Some(identity);
        }
    }
}

/// Bounded unauthenticated HEAD at the exact canonical repo URL. Returns the
/// mapped identity iff the 302 Location equals an allowlisted
/// `<base>/<path>` exactly. Anything else (timeout, denial, malformed,
/// unknown base) yields unknown. No authorization is ever sent; the target
/// is never contacted (no redirect followed); no payload is read.
fn lookup_primary(
    source: &PrimarySource,
    path: &str,
    bound_secs: u64,
    root: &Path,
) -> Option<String> {
    let url = format!("{}/{path}", source.pointer_base);
    if url.contains('@') || url.contains(['?', '#', ' ', '\t', '\n', '\0']) {
        return None;
    }
    let scheme = if source.pointer_base.starts_with("https://") {
        "=https"
    } else {
        "=http"
    };
    let mut environment = Environment::new();
    for name in ["PATH", "TMPDIR", "SSL_CERT_FILE", "SSL_CERT_DIR"] {
        if let Some(value) = std::env::var_os(name) {
            environment.insert(name.into(), value);
        }
    }
    let runner = Runner::new(
        root.to_path_buf(),
        environment,
        Duration::from_secs(bound_secs.max(1)),
    )
    .ok()?
    .with_stderr_events()
    .without_child_stderr();
    let args: Vec<String> = [
        "curl",
        "--disable",
        "--silent",
        "--globoff",
        "--no-netrc",
        "--connect-timeout",
        "10",
        "--max-time",
        &bound_secs.max(1).to_string(),
        "--proto",
        scheme,
        "--head",
        "--output",
        "/dev/null",
        "--write-out",
        "%{http_code} %{redirect_url}",
        "--url",
        &url,
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let stdout = runner.run(&args, true).ok()?;
    // `%{redirect_url}` is empty unless a Location was received; a relative
    // Location arrives resolved against the request URL. Header size itself
    // is governed by libcurl's internal limits; only these two write-out
    // values are ever read (stderr discarded, bodies never touched).
    // The redirect may legally contain spaces, so the code is the first
    // token and everything after it is the Location.
    let (code, location) = stdout.split_once(' ')?;
    if code != "302" || location.is_empty() {
        return None;
    }
    // Exact match only: allowlisted base + the requested path, nothing else.
    for (base, identity) in &source.identities {
        if location == format!("{base}/{path}") {
            return Some(identity.clone());
        }
    }
    None
}

struct CliProber {
    stores: Vec<Store>,
    timeout_secs: u64,
    root: PathBuf,
}

impl Prober for CliProber {
    fn probe(&self, store: usize, path: &str, need: &RefKind) -> Outcome {
        let Some(target) = self.stores.get(store) else {
            return Outcome::Error;
        };
        match target.kind {
            clmr::StoreKind::HttpForge => self.http_probe(target, path, need),
            clmr::StoreKind::Filesystem => self.fs_probe(target, path, need),
        }
    }
}

/// Credential posture for one store probe. A token is used only when a
/// valid reference names a set, well-formed value; otherwise the probe is
/// anonymous (public jobs carry no token). Malformed references or values
/// fail closed instead of silently downgrading.
enum Credential {
    Token(String),
    Anonymous,
}

fn credential_for(store: &Store) -> Result<Credential> {
    let Some(name) = store.credential_env.as_deref() else {
        return Ok(Credential::Anonymous);
    };
    if !name.starts_with("CFRG_RESOLVER_")
        || !name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err("Invalid resolver credential reference".into());
    }
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(Credential::Anonymous),
        Err(std::env::VarError::NotUnicode(_)) => Err("Invalid resolver credential".into()),
        Ok(token) => {
            if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
                return Err("Invalid resolver credential".into());
            }
            Ok(Credential::Token(token))
        }
    }
}

fn token_from(env_name: &str) -> Result<String> {
    if !env_name.starts_with("CFRG_RESOLVER_")
        || !env_name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err("Invalid resolver credential reference".into());
    }
    let token = std::env::var(env_name).map_err(|error| match error {
        std::env::VarError::NotPresent => "Missing resolver credential".to_string(),
        std::env::VarError::NotUnicode(_) => "Invalid resolver credential".to_string(),
    })?;
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("Invalid resolver credential".into());
    }
    Ok(token)
}

fn api_path(path: &str, need: &RefKind) -> Option<String> {
    match need {
        RefKind::Pinned(sha) => Some(cfgj::STATUS.commit_path(path, sha)),
        RefKind::Moving(gitref) => {
            let (api, name) = gitref
                .strip_prefix("refs/heads/")
                .map(|b| ("branches", b))
                .or_else(|| gitref.strip_prefix("refs/tags/").map(|t| ("tags", t)))?;
            Some(format!(
                "/api/v1/repos/{path}/{api}/{}",
                encode_segment(name)
            ))
        }
    }
}

/// Percent-encode one API path segment. Ref names are pre-validated to
/// alphanumerics plus `/._-+`, so only `/` and `+` need encoding.
fn encode_segment(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'/' => out.push_str("%2F"),
            b'+' => out.push_str("%2B"),
            _ => out.push(byte as char),
        }
    }
    out
}

impl CliProber {
    fn http_probe(&self, store: &Store, path: &str, need: &RefKind) -> Outcome {
        // Only the Forgejo probe kind exists. Anything else is skipped here
        // (no hit, never guessed via ls-remote: ls-remote cannot prove a
        // pinned commit exists).
        if store.provider.as_deref() != Some("forgejo") {
            return Outcome::Unsupported;
        }
        let Some(suffix) = api_path(path, need) else {
            return Outcome::Unsupported;
        };
        let credential = match credential_for(store) {
            Ok(credential) => credential,
            Err(_) => return Outcome::Error,
        };
        let scheme = if store.location.starts_with("https://") {
            "=https"
        } else {
            "=http,https"
        };
        let mut environment = Environment::new();
        for name in ["PATH", "TMPDIR", "SSL_CERT_FILE", "SSL_CERT_DIR"] {
            if let Some(value) = std::env::var_os(name) {
                environment.insert(name.into(), value);
            }
        }
        // Authenticated only with a set, well-formed token: the variable and
        // header args below exist solely in that case. Anonymous probes send
        // no Authorization and no token-carrying curl arguments at all.
        let authenticated = matches!(credential, Credential::Token(_));
        if let Credential::Token(token) = credential {
            environment.insert("CFRG_RESOLVER_TOKEN".into(), token.into());
        }
        let timeout = Duration::from_secs(self.timeout_secs);
        let runner = match Runner::new(self.root.clone(), environment, timeout) {
            Ok(runner) => runner.with_stderr_events().without_child_stderr(),
            Err(_) => return Outcome::Error,
        };
        let output = match tempfile::NamedTempFile::new() {
            Ok(file) => file,
            Err(_) => return Outcome::Error,
        };
        let headers = match tempfile::NamedTempFile::new() {
            Ok(file) => file,
            Err(_) => return Outcome::Error,
        };
        let mut parts: Vec<String> = [
            "curl",
            "--disable",
            "--silent",
            "--globoff",
            "--no-netrc",
            "--connect-timeout",
            "10",
            "--max-time",
            &self.timeout_secs.to_string(),
            "--max-filesize",
            "4194304",
            "--proto",
            scheme,
        ]
        .into_iter()
        .map(String::from)
        .collect();
        if authenticated {
            parts.extend(
                [
                    "--variable",
                    "%CFRG_RESOLVER_TOKEN",
                    "--expand-header",
                    "Authorization: token {{CFRG_RESOLVER_TOKEN}}",
                ]
                .into_iter()
                .map(String::from),
            );
        }
        parts.extend(
            [
                "--header",
                "Accept: application/json",
                "--output",
                &output.path().display().to_string(),
                "--dump-header",
                &headers.path().display().to_string(),
                "--write-out",
                "%{http_code}",
                "--url",
                &format!("{}{suffix}", store.location),
            ]
            .into_iter()
            .map(String::from),
        );
        let args: Vec<String> = parts;
        let code = match runner.run(&args, true) {
            Ok(code) => code,
            Err(_) => return Outcome::Error,
        };
        // Never print bodies or curl diagnostics: they may carry
        // credential-bearing input. Only the status class leaves this fn.
        match code.parse::<u16>().unwrap_or_default() {
            200 => (),
            404 => return Outcome::Miss,
            401 | 403 => return Outcome::Denied,
            _ => return Outcome::Error,
        }
        let body: serde_json::Value = match std::fs::read(output.path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            Some(body) => body,
            None => return Outcome::Error,
        };
        let hit = match need {
            RefKind::Pinned(sha) => body.get("sha").and_then(|v| v.as_str()) == Some(sha.as_str()),
            RefKind::Moving(gitref) => {
                let name = gitref
                    .strip_prefix("refs/heads/")
                    .or_else(|| gitref.strip_prefix("refs/tags/"))
                    .unwrap_or_default();
                body.get("name").and_then(|v| v.as_str()) == Some(name)
            }
        };
        if hit {
            Outcome::Hit
        } else {
            Outcome::Miss
        }
    }

    fn fs_probe(&self, store: &Store, path: &str, need: &RefKind) -> Outcome {
        let root = Path::new(&store.location);
        let canonical_root = match root.canonicalize() {
            Ok(canonical) => canonical,
            Err(_) => return Outcome::Error,
        };
        // The contract probes exactly `{root}/{path}.git`: the same
        // repository the renderer emits as `via`. No fallback form exists,
        // so presence and emission can never designate different repos.
        match self.fs_candidate(&canonical_root, root, &format!("{path}.git"), need) {
            Some(outcome) => outcome,
            None => Outcome::Miss,
        }
    }

    /// `None` = candidate absent; `Some` = definitive answer.
    fn fs_candidate(
        &self,
        canonical_root: &Path,
        root: &Path,
        candidate: &str,
        need: &RefKind,
    ) -> Option<Outcome> {
        let joined = root.join(candidate);
        // Reject every symlink on the resolved chain before touching git.
        let mut prefix = PathBuf::from(root);
        let Ok(relative) = joined.strip_prefix(root) else {
            return Some(Outcome::Miss);
        };
        for component in relative.components() {
            prefix.push(component);
            match std::fs::symlink_metadata(&prefix) {
                Ok(meta) if meta.file_type().is_symlink() => return Some(Outcome::Miss),
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
                Err(_) => return Some(Outcome::Error),
            }
        }
        let canonical = match joined.canonicalize() {
            Ok(canonical) => canonical,
            Err(_) => return None,
        };
        if !canonical.starts_with(canonical_root) {
            return Some(Outcome::Miss);
        }
        // Git metadata symlinks would redirect the probe outside the
        // chain verified above; confinement claims cover these explicitly.
        for guarded in ["config", "objects", "HEAD"] {
            match std::fs::symlink_metadata(canonical.join(guarded)) {
                Ok(meta) if meta.file_type().is_symlink() => return Some(Outcome::Miss),
                Ok(_) => (),
                Err(_) => return Some(Outcome::Miss),
            }
        }
        if !canonical.join("objects").is_dir() || !canonical.join("HEAD").is_file() {
            return Some(Outcome::Miss);
        }
        // Promisor/lazy content must never satisfy a probe.
        if canonical.join("objects/info/alternates").exists() {
            return Some(Outcome::Miss);
        }
        // Config read errors fail closed (pointer), never blind probing.
        match std::fs::metadata(canonical.join("config")) {
            Ok(meta) if meta.len() <= 65536 => (),
            _ => return Some(Outcome::Miss),
        }
        match std::fs::read_to_string(canonical.join("config")) {
            Ok(text) => {
                let lower = text.to_ascii_lowercase();
                if FORBIDDEN_CONFIG_KEYS.iter().any(|key| lower.contains(key)) {
                    return Some(Outcome::Miss);
                }
            }
            Err(_) => return Some(Outcome::Miss),
        }
        // Inherited Git settings (notably GIT_CONFIG_COUNT/KEY_n/VALUE_n)
        // would bypass the hardened config below: drop ALL GIT_* first.
        let mut environment: Environment = std::env::vars_os().collect();
        environment.retain(|key, _| !key.to_string_lossy().starts_with("GIT_"));
        for (key, value) in [
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_LFS_SKIP_SMUDGE", "1"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
            ("GIT_ATTR_NOSYSTEM", "1"),
            ("GIT_PROTOCOL_FROM_USER", "0"),
            ("LC_ALL", "C"),
        ] {
            environment.insert(key.into(), value.into());
        }
        let runner = match Runner::new(
            self.root.clone(),
            environment,
            Duration::from_secs(self.timeout_secs),
        ) {
            Ok(runner) => runner.with_stderr_events().without_child_stderr(),
            Err(_) => return Some(Outcome::Error),
        };
        let dir = canonical.display().to_string();
        let mut argv = vec![
            "git".to_owned(),
            "-c".into(),
            "core.hooksPath=/dev/null".into(),
            "-c".into(),
            "protocol.file.allow=never".into(),
            "-c".into(),
            format!("safe.directory={dir}"),
            "--git-dir".into(),
            dir,
        ];
        match need {
            RefKind::Pinned(sha) => {
                // `-t` plus exact output: `-e` proves any object exists
                // (blob included), but a pinned fetch needs a commit.
                argv.extend(["cat-file".into(), "-t".into(), sha.clone()]);
            }
            RefKind::Moving(gitref) => {
                argv.extend(["show-ref".into(), "--verify".into(), gitref.clone()]);
            }
        }
        match runner.run(&argv, true) {
            Ok(output) => match need {
                RefKind::Pinned(_) if output == "commit" => Some(Outcome::Hit),
                RefKind::Pinned(_) => Some(Outcome::Miss),
                RefKind::Moving(_) => Some(Outcome::Hit),
            },
            Err(error) => {
                let message = error.to_string();
                if message.contains("timed out")
                    || message.contains("deadline exceeded")
                    || message.contains("interrupted")
                {
                    Some(Outcome::Error)
                } else {
                    Some(Outcome::Miss)
                }
            }
        }
    }
}

/// Git credential helper protocol: `cfrg credential-helper --env E
/// --expect-origin O [--username U] get`. Only `get` is answered; `store`
/// and `erase` are no-ops. The token travels env -> stdout only, never
/// argv/log/config, and only for the exact normalized origin.
pub fn run_credential_helper(
    env: String,
    expect_origin: String,
    username: Option<String>,
    action: String,
) -> Result<()> {
    if action != "get" {
        return Ok(());
    }
    let expected = clmr::normalize_origin(&expect_origin)
        .ok_or_else(|| "Invalid credential helper origin".to_string())?;
    let mut input = Vec::new();
    std::io::stdin()
        .take(65537)
        .read_to_end(&mut input)
        .map_err(|_| "Cannot read credential request".to_string())?;
    if input.len() > 65536 {
        return Err("Credential request too large".into());
    }
    let text = String::from_utf8(input).map_err(|_| "Invalid credential request".to_string())?;
    let mut protocol = None;
    let mut host = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "protocol" => protocol = Some(value.trim().to_owned()),
            "host" => host = Some(value.trim().to_owned()),
            _ => (),
        }
    }
    let actual = clmr::normalize_origin(&format!(
        "{}://{}",
        protocol.as_deref().unwrap_or_default(),
        host.as_deref().unwrap_or_default()
    ))
    .ok_or_else(|| "Invalid credential request origin".to_string())?;
    if actual != expected {
        return Err("Credential helper refuses foreign origin".into());
    }
    let token = token_from(&env)?;
    if let Some(user) = username {
        if !clmr::valid_username(&user) {
            return Err("Invalid credential helper username".into());
        }
        println!("username={user}");
    }
    // Token to stdout: git's pipe, not a log. Nothing else prints it.
    println!("password={token}");
    Ok(())
}
