//! Bounded, paced transport with durable rate windows and uncertain-write holds.
use super::{env_name, Endpoint};
use crate::{
    bridge::http::{environment, token},
    failure,
    process::Runner,
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy)]
pub enum Auth {
    Token,
    PrivateToken,
    Bearer,
}

/// Constructed only by adapters, never accepted as arbitrary CLI input.
#[derive(Clone)]
pub struct Request {
    pub endpoint: Endpoint,
    pub auth: Auth,
    pub method: &'static str,
    pub path: String,
    pub body: Option<Value>,
    pub scope: String,
    pub creation: bool,
}
pub struct Response {
    pub status: u16,
    pub body: Value,
}
pub trait Transport {
    fn send(&mut self, request: Request) -> Result<Response>;
    /// Drop the recorded uncertain-write intent for `scope`. Call only after
    /// the provider state was read back and the outcome is known.
    fn resolve(&mut self, _scope: &str) -> Result<()> {
        Ok(())
    }
    /// Record a request that must never be repeated, or the pacing owed after it, in the durable state. Called right before
    /// the request is sent, so a crash after it cannot repeat it. `github-transfer` sets the request and the next-transfer
    /// time (`value` plus the three minutes the pacing policy asks); `github-transfer-clear` forgets the request.
    fn note(&mut self, _kind: &str, _key: &str, _value: &str) -> Result<()> {
        Ok(())
    }
    /// What `note` recorded: `bitbucket-transfer` (old uuid to the new one), `bitbucket-transfer-by-new` (the old uuid of a new
    /// one), `github-transfer` (a requested transfer's time).
    fn recorded(&self, _kind: &str, _key: &str) -> Option<String> {
        None
    }
    /// A credential by its environment name (the credentials directory first, the process environment as the fallback).
    fn credential(&self, _env: &str, _path: &str) -> Option<String> {
        None
    }
}

/// Minimum spacing between two GitHub repository transfers (the pacing policy: at least three minutes).
pub const GITHUB_TRANSFER_SPACING: u64 = 180;
const _: () = assert!(GITHUB_TRANSFER_SPACING >= 180);

/// Minimum spacing, in seconds, between requests. Reads never shorten the
/// spacing owed after a write or a creation.
#[derive(Clone, Copy, Debug)]
pub struct Pacing {
    pub read: u64,
    pub write: u64,
    pub creation: u64,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            read: 2,
            write: 12,
            creation: 60,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    /// Ownership is derived, never stored (design 3.7). An older state file may still carry the
    /// retired key: it is read, dropped, and never written back.
    #[serde(default, skip_serializing, rename = "owned")]
    #[allow(dead_code)]
    retired_owned: serde::de::IgnoredAny,
    pub windows: BTreeMap<String, Window>,
    pub pending: BTreeMap<String, String>,
    pub next_request: u64,
    #[serde(default)]
    pub next_mutation: u64,
    pub next_creation: u64,
    /// Earliest time of the next GitHub repository transfer (the pacing policy: at least three minutes apart). Kept here, not
    /// slept on: a transfer that is not yet due is reported `transfer-blocked: paced`.
    #[serde(default)]
    pub next_github_transfer: u64,
    /// GitHub repository id to the time its transfer was requested and not yet seen at the new owner: never requested twice.
    #[serde(default)]
    pub github_transfers: BTreeMap<String, u64>,
    /// Bitbucket old repository uuid to the uuid of the repository created for its move. The description marker of the new
    /// repository can be rewritten by `settings`; this record cannot.
    #[serde(default)]
    pub bitbucket_transfers: BTreeMap<String, String>,
    /// Old uuids whose Bitbucket move is complete (the old repository is deleted).
    #[serde(default)]
    pub bitbucket_transfers_done: std::collections::BTreeSet<String>,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct Window {
    pub observed_at: u64,
    pub status: u16,
    /// None means operator intervention, not permission to retry.
    pub retry_at: Option<u64>,
}

impl State {
    fn deadline(&self, write: bool, creation: bool) -> u64 {
        self.next_request
            .max(if write { self.next_mutation } else { 0 })
            .max(if creation { self.next_creation } else { 0 })
    }
}

/// Resolves a token by the endpoint's `token_env` name and the request path (a GitHub App token is per owner).
pub type SecretFn = Box<dyn Fn(&str, &str) -> Result<String>>;

pub struct Http {
    state_path: PathBuf,
    lock_path: PathBuf,
    pub state: State,
    count: u32,
    apply: bool,
    pacing: Pacing,
    /// Where a token comes from when it is not in the process environment (a credentials directory).
    secrets: Option<SecretFn>,
    /// Consecutive permission refusals (HTTP 403 without a rate limit) per scope; any answer below 400 resets it.
    refusals: std::collections::BTreeMap<String, u32>,
    /// The curl executable and the pause function: fixed in production, swapped by tests only.
    curl: String,
    sleep: fn(u64),
}

/// Marks a 403 that refuses one request (a permission the credential does not hold, e.g. a GitHub App with
/// `Administration: read` asked to change a setting). It is not a stop: the caller fails that one row.
pub const REFUSED_MARK: &str = "refused for this request";

/// Permission refusals in a row, on one scope and without a success in between, that still end the run: an account
/// that is flagged or suspended answers every request with 403, and the run must not go on asking.
const MAX_CONSECUTIVE_REFUSALS: u32 = 3;

/// Counts one more permission refusal on `scope`; true when it is the one that ends the run.
fn refusal_ends_the_run(
    refusals: &mut std::collections::BTreeMap<String, u32>,
    scope: &str,
) -> bool {
    let streak = refusals.entry(scope.to_string()).or_default();
    *streak += 1;
    *streak >= MAX_CONSECUTIVE_REFUSALS
}

/// Whether the response headers say the 403 is a rate limit (primary: `x-ratelimit-remaining: 0`; secondary: `retry-after`).
fn is_rate_limited(headers: &str) -> bool {
    headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .any(|(key, value)| {
            key.eq_ignore_ascii_case("retry-after")
                || (key.eq_ignore_ascii_case("x-ratelimit-remaining") && value.trim() == "0")
        })
}

pub fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

impl Http {
    pub fn open(path: &Path, apply: bool) -> Result<Self> {
        let lock_path = path.with_extension("lock");
        let mut lock = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .map_err(|_| {
                failure("Native state locked; inspect owner before recovering a stale lock")
            })?;
        writeln!(lock, "{}", std::process::id())?;
        let state = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => {
                fs::remove_file(&lock_path)?;
                return Err(e.into());
            }
        };
        match state {
            Ok(state) => Ok(Self {
                state_path: path.into(),
                lock_path,
                state,
                count: 0,
                apply,
                pacing: Pacing::default(),
                secrets: None,
                refusals: Default::default(),
                curl: "curl".into(),
                sleep: |seconds| std::thread::sleep(Duration::from_secs(seconds)),
            }),
            Err(e) => {
                fs::remove_file(&lock_path)?;
                Err(e.into())
            }
        }
    }
    /// A credential by its environment name: the credentials directory first, the process environment as the fallback of a hand run.
    pub fn credential(&self, env: &str, path: &str) -> Option<String> {
        if let Some(resolve) = &self.secrets {
            if let Ok(value) = resolve(env, path) {
                return Some(value);
            }
        }
        std::env::var(env).ok()
    }

    /// Resolve tokens through `secrets` (by the endpoint's `token_env` name) instead of the environment.
    pub fn with_secrets(mut self, secrets: SecretFn) -> Self {
        self.secrets = Some(secrets);
        self
    }
    /// Use a tighter or looser request spacing than the conservative default.
    pub fn paced(mut self, pacing: Pacing) -> Self {
        self.pacing = pacing;
        self
    }
    /// Change the request spacing in the middle of a run (the copy pass talks to
    /// our own forge only and needs no provider spacing).
    pub fn set_pacing(&mut self, pacing: Pacing) {
        self.pacing = pacing;
    }
    pub fn save(&self) -> Result<()> {
        let parent = self
            .state_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut file, &self.state)?;
        file.as_file().sync_all()?;
        file.persist(&self.state_path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// A read is tried at most this many times (one try plus the retries) when the failure is transient.
const READ_ATTEMPTS: u32 = 5;
/// The pause before retry `n` (0-based) doubles from 2 s and never exceeds this many seconds, jitter included.
const READ_PAUSE_CAP: u64 = 30;

/// Exponential pause before retry `retry` with up to half again of jitter, capped at `READ_PAUSE_CAP`.
fn backoff(retry: u32, jitter: u64) -> u64 {
    let base = (2u64 << retry.min(10)).min(READ_PAUSE_CAP);
    (base + jitter % (base / 2 + 1)).min(READ_PAUSE_CAP)
}

fn jitter_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0)
}

/// curl exit codes of a transport that failed before or during an answer: unresolved proxy/host (5, 6), refused
/// (7), partial file (18), timeout (28), TLS connect (35), empty reply (52), send/recv failure and reset (55, 56).
const TRANSIENT_CURL_EXITS: [u32; 9] = [5, 6, 7, 18, 28, 35, 52, 55, 56];

/// Why a failed read is worth another try, if it is: a 5xx, or a transport failure of curl. Everything else
/// (4xx, a malformed answer, a refused credential) is final.
fn transient_reason(error: &dyn std::error::Error) -> Option<String> {
    let text = error.to_string();
    if let Some(rest) = text.strip_prefix("Native API HTTP 5") {
        return Some(format!("http-5{}", &rest[..rest.len().min(2)]));
    }
    let code = text.strip_prefix("curl failed: exit status: ")?;
    let code: u32 = code.split_whitespace().next()?.parse().ok()?;
    TRANSIENT_CURL_EXITS
        .contains(&code)
        .then(|| format!("curl-exit-{code}"))
}
/// A throttled read (429) is retried at most this many times, and only when the provider's Retry-After is at most
/// this many seconds; a longer window stays the recorded hold it always was.
const THROTTLE_RETRIES: u32 = 2;
const THROTTLE_MAX_WAIT: u64 = 120;

/// Run `attempt`; a failure for which `transient` holds is run again after each pause of
/// `pauses` (through `sleep`), so a flaky server costs time and never the whole run.
fn retried<T>(
    pauses: &[u64],
    sleep: &mut dyn FnMut(u64),
    transient: impl Fn(&dyn std::error::Error) -> Option<String>,
    mut attempt: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut pauses = pauses.iter();
    let mut tries = 0u32;
    loop {
        tries += 1;
        match attempt() {
            Err(error) => match (transient(error.as_ref()), pauses.next()) {
                (Some(reason), Some(seconds)) => {
                    // Reason and counts only: never a URL, header or token.
                    eprintln!(
                        "{}",
                        serde_json::json!({"event":"native-retry","attempt":tries,"reason":reason,"after_seconds":seconds})
                    );
                    sleep(*seconds);
                }
                _ => return Err(error),
            },
            other => return other,
        }
    }
}

impl Transport for Http {
    fn send(&mut self, request: Request) -> Result<Response> {
        // A read has no side effect, so a server error is retried; a write never is (it may
        // have been committed), and no 5xx records a window.
        if request.method != "GET" {
            return self.send_once(request);
        }
        let seed = jitter_seed();
        let pauses: Vec<u64> = (0..READ_ATTEMPTS - 1)
            .map(|n| backoff(n, seed.rotate_left(n * 7)))
            .collect();
        let sleep = self.sleep;
        let mut throttled = 0;
        loop {
            let result = retried(
                &pauses,
                &mut |seconds| sleep(seconds),
                transient_reason,
                || self.send_once(request.clone()),
            );
            // A throttled read (429) is retried once the provider's own Retry-After has passed, when that is
            // short: the window is already recorded, so the retry waits for exactly the time the provider asked for.
            let Err(error) = &result else { return result };
            if throttled >= THROTTLE_RETRIES
                || !error.to_string().starts_with("Native API HTTP 429")
            {
                return result;
            }
            let wait = match (self.state.windows.get(&request.scope), now()) {
                (Some(window), Ok(at)) => window.retry_at.map(|until| until.saturating_sub(at)),
                _ => None,
            };
            match wait {
                Some(seconds) if seconds <= THROTTLE_MAX_WAIT => {
                    eprintln!(
                        "{}",
                        serde_json::json!({"event":"native-retry","status":429,"after_seconds":seconds + 1})
                    );
                    std::thread::sleep(Duration::from_secs(seconds + 1));
                    throttled += 1;
                }
                _ => return result,
            }
        }
    }

    fn resolve(&mut self, scope: &str) -> Result<()> {
        self.state.pending.remove(scope);
        self.save()
    }

    fn note(&mut self, kind: &str, key: &str, value: &str) -> Result<()> {
        match kind {
            "github-transfer" => {
                let at: u64 = value
                    .parse()
                    .map_err(|_| failure("Invalid transfer time"))?;
                self.state.github_transfers.insert(key.to_string(), at);
                self.state.next_github_transfer = at + GITHUB_TRANSFER_SPACING;
            }
            "github-transfer-clear" => {
                self.state.github_transfers.remove(key);
            }
            "bitbucket-transfer" => {
                self.state
                    .bitbucket_transfers
                    .insert(key.to_string(), value.to_string());
            }
            "bitbucket-transfer-done" => {
                self.state.bitbucket_transfers_done.insert(key.to_string());
            }
            _ => return Err(failure("Unknown state note")),
        }
        self.save()
    }

    fn recorded(&self, kind: &str, key: &str) -> Option<String> {
        match kind {
            "github-transfer" => self
                .state
                .github_transfers
                .get(key)
                .map(|at| at.to_string()),
            "bitbucket-transfer" => self.state.bitbucket_transfers.get(key).cloned(),
            "bitbucket-transfer-done" => self
                .state
                .bitbucket_transfers_done
                .contains(key)
                .then(|| "1".to_string()),
            "bitbucket-transfer-by-new" => self
                .state
                .bitbucket_transfers
                .iter()
                .find(|(_, new)| new.as_str() == key)
                .map(|(old, _)| old.clone()),
            _ => None,
        }
    }

    fn credential(&self, env: &str, path: &str) -> Option<String> {
        Http::credential(self, env, path)
    }
}

impl Http {
    fn send_once(&mut self, request: Request) -> Result<Response> {
        request.endpoint.validate()?;
        env_name(&request.endpoint.token_env)?;
        crate::bridge::http::reject_path_tricks(&request.path)?;
        let write = request.method != "GET";
        if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&request.method)
            || (write && !self.apply)
        {
            return Err(failure("Native mutation requires apply mode"));
        }
        if self.count >= 10000 {
            return Err(failure("Native request budget exhausted"));
        }
        let timestamp = now()?;
        if let Some(window) = self.state.windows.get(&request.scope) {
            if (window.status != 402 || write)
                && window.retry_at.is_none_or(|until| timestamp < until)
            {
                return Err(failure(format!(
                    "Native rate/plan hold for {}; inspect recorded window",
                    request.scope
                )));
            }
        }
        if write && self.state.pending.contains_key(&request.scope) {
            return Err(failure("Uncertain prior write; read provider state and resolve the recorded intent before retrying"));
        }
        let deadline = self.state.deadline(write, request.creation);
        if deadline.saturating_sub(timestamp) > 120 {
            return Err(failure("Pacing deadline is invalid or not yet due"));
        }
        std::thread::sleep(Duration::from_secs(deadline.saturating_sub(timestamp)));
        let timestamp = now()?;
        self.state.next_request = timestamp + self.pacing.read;
        if write {
            self.state.next_mutation = timestamp + self.pacing.write;
        }
        if request.creation {
            self.state.next_creation = timestamp + self.pacing.creation;
        }
        if write {
            self.state.pending.insert(
                request.scope.clone(),
                format!("{} {} at {}", request.method, request.path, timestamp),
            );
        }
        self.save()?;
        self.count += 1;
        if write {
            eprintln!(
                "{}",
                serde_json::json!({"event":"native-intent","method":request.method,"path":request.path,"scope":request.scope})
            );
        }
        let mut env = environment();
        let secret = match &self.secrets {
            Some(resolve) => resolve(&request.endpoint.token_env, &request.path)?,
            None => token(&request.endpoint.token_env)?,
        };
        env.insert("CFRG_NATIVE_TOKEN".into(), secret.into());
        let runner = Runner::new(std::env::current_dir()?, env, Duration::from_secs(45))?
            .with_stderr_events()
            .without_child_stderr();
        let output = tempfile::NamedTempFile::new()?;
        let headers = tempfile::NamedTempFile::new()?;
        let mut input = tempfile::NamedTempFile::new()?;
        let header = match request.auth {
            Auth::Token => "Authorization: token {{CFRG_NATIVE_TOKEN}}",
            Auth::PrivateToken => "PRIVATE-TOKEN: {{CFRG_NATIVE_TOKEN}}",
            Auth::Bearer => "Authorization: Bearer {{CFRG_NATIVE_TOKEN}}",
        };
        let mut args: Vec<String> = [
            self.curl.as_str(),
            "--disable",
            "--silent",
            "--globoff",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--max-filesize",
            "4194304",
            "--proto",
            "=https",
            "--variable",
            "%CFRG_NATIVE_TOKEN",
            "--expand-header",
            header,
            "--header",
            "Accept: application/json",
            "--output",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        args.push(output.path().to_string_lossy().into_owned());
        args.extend([
            "--dump-header".into(),
            headers.path().to_string_lossy().into_owned(),
            "--write-out".into(),
            "%{http_code}".into(),
            "--request".into(),
            request.method.into(),
            "--url".into(),
            format!("{}{}", request.endpoint.origin, request.path),
        ]);
        if let Some(body) = request.body {
            serde_json::to_writer(&mut input, &body)?;
            input.flush()?;
            args.extend([
                "--header".into(),
                "Content-Type: application/json".into(),
                "--data-binary".into(),
                format!("@{}", input.path().display()),
            ]);
        }
        // No redirects, ambient credentials/proxies, stderr, retries or response-body diagnostics.
        let status: u16 = runner
            .run(&args, true)?
            .trim()
            .parse()
            .map_err(|_| failure("Unknown HTTP outcome"))?;
        if status == 0 {
            return Err(failure("Unknown HTTP outcome"));
        }
        eprintln!(
            "{}",
            serde_json::json!({"event":"native-response","method":request.method,"path":request.path,"status":status,"scope":request.scope})
        );
        // A server error on a mutation may have occurred after it committed.
        if write && status < 500 {
            self.state.pending.remove(&request.scope);
        }
        let headers_text = fs::read_to_string(headers.path())?;
        let rate_limited_403 = status == 403 && is_rate_limited(&headers_text);
        if status < 400 {
            self.refusals.remove(&request.scope);
        }
        if status == 429 || status == 402 || rate_limited_403 {
            let observed_at = now()?;
            let retry_at = if status == 402 {
                None
            } else {
                retry_after(&headers_text, observed_at)
            };
            self.state.windows.insert(
                request.scope.clone(),
                Window {
                    observed_at,
                    status,
                    retry_at,
                },
            );
        }
        self.save()?;
        if status == 403
            && !rate_limited_403
            && !refusal_ends_the_run(&mut self.refusals, &request.scope)
        {
            return Err(failure(format!(
                "Native API HTTP 403; {REFUSED_MARK}; the run goes on"
            )));
        }
        if matches!(status, 401 | 403 | 429 | 402) || status >= 500 {
            return Err(failure(format!(
                "Native API HTTP {status}; stopped without retry; window/intent recorded"
            )));
        }
        let bytes = fs::read(output.path())?;
        if status == 422 {
            // Static diagnostic only, never the provider's potentially secret body.
            let kind = if String::from_utf8_lossy(&bytes).contains("already been taken") {
                "duplicate-value"
            } else {
                "validation-rejected"
            };
            eprintln!(
                "{}",
                serde_json::json!({"event":"native-rejection","status":status,"kind":kind})
            );
        }
        // Never parse or echo error bodies (may contain credentials).
        let body = if (200..300).contains(&status) && !bytes.is_empty() {
            serde_json::from_slice(&bytes)
                .map_err(|_| failure("Malformed native API success response"))?
        } else {
            Value::Null
        };
        Ok(Response { status, body })
    }
}

pub fn expect(response: Response, statuses: &[u16]) -> Result<Value> {
    if !statuses.contains(&response.status) {
        return Err(failure(format!(
            "Native API HTTP {}; no retry",
            response.status
        )));
    }
    Ok(response.body)
}

fn retry_after(headers: &str, now: u64) -> Option<u64> {
    // HTTP-date or missing/invalid delays remain held until explicit review.
    headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .rfind(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .and_then(|delay| now.checked_add(delay))
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_credential_comes_from_the_directory_resolver_before_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let http = Http::open(&dir.path().join("state.json"), false)
            .unwrap()
            .with_secrets(Box::new(|env, _| {
                if env == "CFRG_TEST_FROM_DIR" {
                    Ok("from-dir".to_string())
                } else {
                    Err("none".into())
                }
            }));
        assert_eq!(
            http.credential("CFRG_TEST_FROM_DIR", "a/b").as_deref(),
            Some("from-dir")
        );
        assert_eq!(http.credential("CFRG_TEST_ABSENT_9z", "a/b"), None);
    }
    use super::*;
    #[test]
    fn a_403_without_a_rate_limit_fails_one_request_and_three_in_a_row_end_the_run() {
        let mut refusals = std::collections::BTreeMap::new();
        assert!(!refusal_ends_the_run(&mut refusals, "github/corbet-libre"));
        assert!(!refusal_ends_the_run(&mut refusals, "github/corbet-libre"));
        // another scope has its own streak
        assert!(!refusal_ends_the_run(&mut refusals, "gitlab/x"));
        assert!(refusal_ends_the_run(&mut refusals, "github/corbet-libre"));
        // the refusal message must not look like a stop to the callers that match on it
        assert!(
            !format!("Native API HTTP 403; {REFUSED_MARK}; the run goes on")
                .contains("stopped without retry")
        );
    }
    #[test]
    fn only_rate_limit_headers_make_a_403_a_rate_limit() {
        assert!(is_rate_limited("HTTP/2 403\r\nRetry-After: 60\r\n"));
        assert!(is_rate_limited("x-ratelimit-remaining: 0\r\n"));
        assert!(!is_rate_limited(
            "x-ratelimit-remaining: 4999\r\ncontent-type: application/json\r\n"
        ));
        assert!(!is_rate_limited("HTTP/2 403\r\n"));
    }
    #[test]
    fn reads_cannot_shorten_write_or_creation_pacing() {
        let mut state = State {
            next_request: 102,
            next_mutation: 112,
            next_creation: 160,
            ..State::default()
        };
        assert_eq!(state.deadline(false, false), 102);
        state.next_request = 107;
        assert_eq!(state.deadline(true, false), 112);
        assert_eq!(state.deadline(true, true), 160);
    }
    #[test]
    fn transfer_records_survive_a_save_and_an_old_state_file_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"windows":{},"pending":{},"next_request":0,"next_creation":0}"#,
        )
        .unwrap();
        let mut http = Http::open(&path, true).unwrap();
        http.note("github-transfer", "5", "1000").unwrap();
        http.note("bitbucket-transfer", "{old}", "{new}").unwrap();
        assert_eq!(
            http.state.next_github_transfer,
            1000 + GITHUB_TRANSFER_SPACING
        );
        drop(http);
        let mut http = Http::open(&path, true).unwrap();
        assert_eq!(
            http.recorded("github-transfer", "5").as_deref(),
            Some("1000")
        );
        assert_eq!(
            http.recorded("bitbucket-transfer", "{old}").as_deref(),
            Some("{new}")
        );
        assert_eq!(
            http.recorded("bitbucket-transfer-by-new", "{new}")
                .as_deref(),
            Some("{old}")
        );
        http.note("github-transfer-clear", "5", "").unwrap();
        assert!(http.recorded("github-transfer", "5").is_none());
        assert!(http.note("nonsense", "k", "v").is_err());
    }
    #[test]
    fn windows_do_not_guess_missing_or_date_delays() {
        assert_eq!(retry_after("Retry-After: 86400\r\n", 100), Some(86500));
        assert_eq!(
            retry_after("Retry-After: Wed, 07 Oct 2026 05:54:02 GMT", 100),
            None
        );
        assert_eq!(retry_after("", 100), None);
    }
    #[test]
    fn exclusive_state_lock_and_pending_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut http = Http::open(&path, true).unwrap();
        assert!(Http::open(&path, true).is_err());
        http.state.pending.insert("create".into(), "unknown".into());
        http.save().unwrap();
        drop(http);
        assert_eq!(
            Http::open(&path, true).unwrap().state.pending["create"],
            "unknown"
        );
    }

    #[test]
    fn recorded_limit_and_unknown_write_refuse_before_credentials_or_network() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut http = Http::open(&path, true).unwrap();
        let request = || Request {
            endpoint: Endpoint {
                origin: "https://forge.invalid".into(),
                token_env: "UNSET_NATIVE_TEST_TOKEN".into(),
            },
            auth: Auth::Token,
            method: "POST",
            path: "/api/repo".into(),
            body: None,
            scope: "create".into(),
            creation: true,
        };
        http.state.windows.insert(
            "create".into(),
            Window {
                observed_at: now().unwrap(),
                status: 429,
                retry_at: None,
            },
        );
        assert!(http
            .send(request())
            .err()
            .unwrap()
            .to_string()
            .contains("rate/plan hold"));
        http.state.windows.clear();
        http.state
            .pending
            .insert("create".into(), "POST unknown".into());
        assert!(http
            .send(request())
            .err()
            .unwrap()
            .to_string()
            .contains("Uncertain prior write"));
    }

    #[test]
    fn a_transient_server_error_is_retried_with_growing_pauses_and_then_gives_up() {
        let transient = transient_reason;
        let mut slept = Vec::new();
        let mut calls = 0;
        let ok = retried(&[5, 20, 60], &mut |s| slept.push(s), transient, || {
            calls += 1;
            if calls < 3 {
                Err(failure("Native API HTTP 500; stopped"))
            } else {
                Ok(calls)
            }
        });
        assert_eq!((ok.unwrap(), slept), (3, vec![5, 20]));
        let mut slept = Vec::new();
        let mut calls = 0;
        let gave_up = retried(
            &[5, 20, 60],
            &mut |s| slept.push(s),
            transient,
            || -> Result<()> {
                calls += 1;
                Err(failure("Native API HTTP 502; stopped"))
            },
        );
        assert!(gave_up.is_err());
        assert_eq!((calls, slept), (4, vec![5, 20, 60]));
        // Anything else (403, 429, a malformed answer) is never retried.
        let mut calls = 0;
        let refused = retried(&[5], &mut |_| {}, transient, || -> Result<()> {
            calls += 1;
            Err(failure("Native API HTTP 403; stopped"))
        });
        assert!(refused.is_err() && calls == 1);
    }

    #[test]
    fn the_pauses_double_with_jitter_and_stay_under_the_cap() {
        assert_eq!(backoff(0, 0), 2);
        assert_eq!(backoff(1, 0), 4);
        assert_eq!(backoff(3, 0), 16);
        for retry in 0..20 {
            for jitter in [0, 1, 7, u64::MAX] {
                assert!(backoff(retry, jitter) <= READ_PAUSE_CAP);
            }
        }
        assert!(backoff(2, 3) >= 8);
    }

    #[test]
    fn only_server_errors_and_transport_failures_are_transient() {
        let reason = |text: &str| transient_reason(failure(text).as_ref());
        assert_eq!(
            reason("Native API HTTP 503; stopped").as_deref(),
            Some("http-503")
        );
        assert_eq!(
            reason("curl failed: exit status: 56").as_deref(),
            Some("curl-exit-56")
        );
        assert!(reason("curl failed: exit status: 35").is_some());
        assert!(reason("curl failed: exit status: 7").is_some());
        assert!(reason("curl failed: exit status: 28").is_some());
        for fatal in [
            "Native API HTTP 401; stopped",
            "Native API HTTP 404; no retry",
            "Native API HTTP 429; stopped",
            "curl failed: exit status: 60",
            "curl failed: exit status: 63",
            "Malformed native API success response",
            "Unknown HTTP outcome",
        ] {
            assert!(reason(fatal).is_none(), "{fatal}");
        }
    }

    /// A curl stand-in: the first `resets` calls exit 56 (connection reset); later calls answer `status`.
    /// Every call appends a line to `calls`.
    #[cfg(unix)]
    fn fake_http(dir: &Path, resets: u32, status: u16) -> Http {
        use std::os::unix::fs::PermissionsExt;
        let calls = dir.join("calls");
        let script = dir.join("curl");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho x >> '{calls}'\nn=$(wc -l < '{calls}')\n\
                 if [ \"$n\" -le {resets} ]; then exit 56; fi\n\
                 while [ $# -gt 0 ]; do case \"$1\" in --output) out=$2;; --dump-header) hdr=$2;; esac; shift; done\n\
                 echo '{{}}' > \"$out\"; : > \"$hdr\"; printf {status}\n",
                calls = calls.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let mut http = Http::open(&dir.join("state.json"), true)
            .unwrap()
            .with_secrets(Box::new(|_, _| Ok("test-token".into())));
        http.curl = script.display().to_string();
        http.sleep = |_| {};
        http
    }

    fn request(method: &'static str) -> Request {
        Request {
            endpoint: Endpoint {
                origin: "https://forge.invalid".into(),
                token_env: "UNSET_NATIVE_TEST_TOKEN".into(),
            },
            auth: Auth::Token,
            method,
            path: "/api/v1/x".into(),
            body: None,
            scope: "read".into(),
            creation: false,
        }
    }

    #[cfg(unix)]
    fn call_count(dir: &Path) -> usize {
        fs::read_to_string(dir.join("calls"))
            .map(|t| t.lines().count())
            .unwrap_or(0)
    }

    #[cfg(unix)]
    #[test]
    fn a_read_survives_resets_and_gives_up_after_five_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let mut http = fake_http(dir.path(), 3, 200);
        let response = http.send(request("GET")).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(call_count(dir.path()), 4);

        let dir = tempfile::tempdir().unwrap();
        let mut http = fake_http(dir.path(), 100, 200);
        assert!(http.send(request("GET")).is_err());
        assert_eq!(call_count(dir.path()), 5);
    }

    #[cfg(unix)]
    #[test]
    fn a_real_error_on_a_read_is_fatal_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut http = fake_http(dir.path(), 0, 404);
        assert_eq!(http.send(request("GET")).unwrap().status, 404);
        assert_eq!(call_count(dir.path()), 1);

        let dir = tempfile::tempdir().unwrap();
        let mut http = fake_http(dir.path(), 0, 401);
        assert!(http.send(request("GET")).is_err());
        assert_eq!(call_count(dir.path()), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_write_is_never_retried() {
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let dir = tempfile::tempdir().unwrap();
            let mut http = fake_http(dir.path(), 100, 200);
            assert!(http.send(request(method)).is_err());
            assert_eq!(call_count(dir.path()), 1, "{method}");
        }
    }
}
