//! `cfrg serve`: the long-running mode. A small webhook receiver (signed
//! deliveries only) marks repositories dirty; reconcile lanes, one worker each,
//! run the forge passes (landing, mirror verification) when a repository is
//! due. Events only bring a pass forward; a slow periodic sweep makes the loop
//! correct even when the forge drops a delivery (Forgejo has no status event
//! and no delivery retry).
//!
//! Std only, safe Rust: a bounded HTTP/1.1 reader, HMAC-SHA256 over the raw
//! body with a constant-time comparison, and a pure schedule that is tested
//! without a clock.
use crate::{failure, native, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const WEBHOOK_PATH: &str = "/hook";

/// Webhook events the receiver understands; registration asks for exactly these.
pub const EVENTS: [&str; 3] = ["push", "pull_request", "pull_request_sync"];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServePolicy {
    /// Address the receiver binds, for example `0.0.0.0:8080`.
    pub listen: String,
    /// Environment variable holding the webhook secret; never in the file.
    pub secret_env: String,
    /// Idle landing sweep: how often a repository without waiting work is
    /// looked at anyway, in case a delivery was lost.
    #[serde(default = "default_sweep")]
    pub sweep_seconds: u64,
    /// Events within this window collapse into one pass.
    #[serde(default = "default_debounce")]
    pub debounce_seconds: u64,
    /// Webhook registration desired state (`cfrg serve --register`).
    pub webhook: Option<Webhook>,
    /// Push mirror verification.
    pub mirrors: Option<MirrorSweep>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    /// Where the forge delivers, for example
    /// `http://cfrg-serve.ci.svc.cluster.local:8080/hook`.
    pub url: String,
    /// Organisations that get one hook for all their repositories; declared
    /// repositories of other owners get a repository hook.
    #[serde(default)]
    pub orgs: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorSweep {
    /// Native placement file (see `cfrg native`) and its request state.
    pub placement: String,
    pub state: String,
    /// Every repository is verified at least this often.
    #[serde(default = "default_mirror_sweep")]
    pub sweep_seconds: u64,
    /// A push to the default branch verifies that repository after this delay,
    /// giving the push mirror time to sync.
    #[serde(default = "default_mirror_delay")]
    pub delay_seconds: u64,
    /// Besides verifying, rename a destination that no longer carries its
    /// primary's name and re-point its mirror (`cfrg native --operation rename
    /// --apply`). Needs the destination credentials in the environment of the
    /// process; without them the lane only reports `rename-pending`.
    #[serde(default)]
    pub rename: bool,
}

fn default_sweep() -> u64 {
    3600
}
fn default_debounce() -> u64 {
    3
}
fn default_mirror_sweep() -> u64 {
    21_600
}
fn default_mirror_delay() -> u64 {
    60
}

impl ServePolicy {
    pub fn validate(&self) -> Result<()> {
        self.listen
            .parse::<SocketAddr>()
            .map_err(|_| failure("serve.listen must be an address like 0.0.0.0:8080"))?;
        native::env_name(&self.secret_env)?;
        if !(60..=86_400).contains(&self.sweep_seconds) || self.debounce_seconds > 60 {
            return Err(failure("serve sweep or debounce out of range"));
        }
        if let Some(webhook) = &self.webhook {
            webhook_url(&webhook.url)?;
            for org in &webhook.orgs {
                native::component(org)?;
            }
        }
        if let Some(mirrors) = &self.mirrors {
            if mirrors.placement.is_empty()
                || mirrors.state.is_empty()
                || !(60..=604_800).contains(&mirrors.sweep_seconds)
                || mirrors.delay_seconds > 3600
            {
                return Err(failure("serve.mirrors is incomplete or out of range"));
            }
        }
        Ok(())
    }
}

fn webhook_url(url: &str) -> Result<()> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| failure("serve.webhook.url must be http(s)"))?;
    let (authority, _) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty()
        || authority.contains('@')
        || !url.bytes().all(|b| b.is_ascii_graphic())
        || !url.ends_with(WEBHOOK_PATH)
    {
        return Err(failure(
            "serve.webhook.url must be a plain URL without credentials ending in /hook",
        ));
    }
    Ok(())
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---------------------------------------------------------------- signature

/// HMAC-SHA256 (RFC 2104) built on the SHA-256 the crate already depends on.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut padded = [0u8; BLOCK];
    if key.len() > BLOCK {
        padded[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        padded[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(padded.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(padded.map(|b| b ^ 0x5c));
    outer.update(inner);
    let mut mac = [0u8; 32];
    mac.copy_from_slice(&outer.finalize());
    mac
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            u8::try_from(hi * 16 + lo).ok()
        })
        .collect()
}

/// `header` is the value of `X-Forgejo-Signature` (hex) or
/// `X-Hub-Signature-256` (`sha256=` and hex) for the raw request body.
pub fn verify_signature(secret: &[u8], body: &[u8], header: &str) -> bool {
    let hex = header.trim();
    let hex = hex.strip_prefix("sha256=").unwrap_or(hex);
    match hex_decode(hex) {
        Some(sent) => constant_time_eq(&sent, &hmac_sha256(secret, body)),
        None => false,
    }
}

// --------------------------------------------------------------------- http

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub header_bytes: usize,
    pub body_bytes: usize,
    pub headers: usize,
    pub total: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            header_bytes: 16 * 1024,
            body_bytes: 4 * 1024 * 1024,
            headers: 64,
            total: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reject {
    BadRequest,
    Timeout,
    LengthRequired,
    BodyTooLarge,
    HeadersTooLarge,
    NotImplemented,
}

impl Reject {
    pub fn status(&self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::Timeout => 408,
            Self::LengthRequired => 411,
            Self::BodyTooLarge => 413,
            Self::HeadersTooLarge => 431,
            Self::NotImplemented => 501,
        }
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Lower-case names.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Read exactly one request: bounded head, bounded `Content-Length` body, no
/// chunked encoding, no header folding, no duplicate lengths.
pub fn read_request(
    stream: &mut impl Read,
    limits: &Limits,
) -> std::result::Result<Request, Reject> {
    let started = Instant::now();
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > limits.header_bytes {
            return Err(Reject::HeadersTooLarge);
        }
        if started.elapsed() > limits.total {
            return Err(Reject::Timeout);
        }
        let read = stream.read(&mut chunk).map_err(|_| Reject::Timeout)?;
        if read == 0 {
            return Err(Reject::BadRequest);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    if head_end > limits.header_bytes {
        return Err(Reject::HeadersTooLarge);
    }
    let head = std::str::from_utf8(&buffer[..head_end]).map_err(|_| Reject::BadRequest)?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or(Reject::BadRequest)?;
    let mut parts = request_line.split(' ');
    let (method, target, version) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v), None) => (m, t, v),
        _ => return Err(Reject::BadRequest),
    };
    if !version.starts_with("HTTP/1.") || !target.starts_with('/') || method.is_empty() {
        return Err(Reject::BadRequest);
    }
    if !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(Reject::BadRequest);
    }
    let path = target.split(['?', '#']).next().unwrap_or("/").to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        if headers.len() >= limits.headers {
            return Err(Reject::HeadersTooLarge);
        }
        if line.starts_with([' ', '\t']) {
            return Err(Reject::BadRequest);
        }
        let (name, value) = line.split_once(':').ok_or(Reject::BadRequest)?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(Reject::BadRequest);
        }
        let name = name.to_ascii_lowercase();
        let value = value.trim().to_owned();
        match headers.get(&name) {
            Some(old) if name == "content-length" && *old != value => {
                return Err(Reject::BadRequest)
            }
            Some(_) if name == "content-length" => {}
            _ => {
                headers.insert(name, value);
            }
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err(Reject::NotImplemented);
    }
    let length = match headers.get("content-length") {
        Some(value) => {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Reject::BadRequest);
            }
            value.parse::<usize>().map_err(|_| Reject::BodyTooLarge)?
        }
        None if method == "POST" => return Err(Reject::LengthRequired),
        None => 0,
    };
    if length > limits.body_bytes {
        return Err(Reject::BodyTooLarge);
    }
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        if started.elapsed() > limits.total {
            return Err(Reject::Timeout);
        }
        let read = stream.read(&mut chunk).map_err(|_| Reject::Timeout)?;
        if read == 0 {
            return Err(Reject::BadRequest);
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(Request {
        method: method.to_owned(),
        path,
        headers,
        body,
    })
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl Response {
    pub fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: format!("{body}\n"),
        }
    }
    pub fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: format!("{value}\n"),
        }
    }
}

pub fn write_response(stream: &mut impl Write, response: &Response) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{}",
        response.status,
        reason,
        response.content_type,
        response.body.len(),
        response.body
    )?;
    stream.flush()
}

// ------------------------------------------------------------------ routing

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    Push { default_branch: bool },
    PullRequest,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Delivery {
    pub repository: String,
    pub kind: Kind,
}

/// Which repository an event is about and whether it matters. Anything else is
/// `None` and ignored.
pub fn parse_delivery(event: &str, body: &Value) -> Option<Delivery> {
    let repository = body["repository"]["full_name"].as_str()?;
    native::path(repository, false).ok()?;
    let kind = if event == "push" {
        let reference = body["ref"].as_str()?;
        let default = body["repository"]["default_branch"].as_str();
        Kind::Push {
            default_branch: default.is_some_and(|d| reference == format!("refs/heads/{d}")),
        }
    } else if event.starts_with("pull_request") {
        Kind::PullRequest
    } else {
        return None;
    };
    Some(Delivery {
        repository: repository.to_owned(),
        kind,
    })
}

// ----------------------------------------------------------------- schedule

/// When each key is next due, in whole seconds. The earliest request wins, so
/// an event can bring a periodic pass forward but never push it back.
#[derive(Debug, Default)]
pub struct Schedule {
    due: BTreeMap<String, u64>,
}

impl Schedule {
    pub fn at(&mut self, key: &str, when: u64) {
        let entry = self.due.entry(key.to_owned()).or_insert(when);
        *entry = (*entry).min(when);
    }

    /// Remove and return the key that is due, the earliest first.
    pub fn pop_due(&mut self, now: u64) -> Option<String> {
        let key = self
            .due
            .iter()
            .filter(|(_, when)| **when <= now)
            .min_by_key(|(key, when)| (**when, (*key).clone()))
            .map(|(key, _)| key.clone())?;
        self.due.remove(&key);
        Some(key)
    }

    pub fn next(&self) -> Option<u64> {
        self.due.values().copied().min()
    }

    pub fn len(&self) -> usize {
        self.due.len()
    }

    pub fn is_empty(&self) -> bool {
        self.due.is_empty()
    }
}

/// What one reconcile pass found and when to look again (`None`: only on the
/// next event).
pub struct Outcome {
    pub summary: Value,
    pub next_in: Option<u64>,
}

pub trait Reconciler: Send {
    fn reconcile(&mut self, key: &str) -> Result<Outcome>;
}

/// After a failed pass the key is retried only after this many seconds.
const ERROR_BACKOFF: u64 = 60;

#[derive(Default)]
struct LaneState {
    schedule: Schedule,
    last: BTreeMap<String, Value>,
    running: Option<String>,
}

/// One worker's queue. A lane runs one pass at a time, so the request state
/// it owns is never contended.
pub struct Lane {
    name: &'static str,
    state: Mutex<LaneState>,
    wake: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Lane {
    pub fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            state: Mutex::new(LaneState::default()),
            wake: Condvar::new(),
        })
    }

    pub fn mark(&self, key: &str, when: u64) {
        lock(&self.state).schedule.at(key, when);
        self.wake.notify_all();
    }

    pub fn status(&self) -> Value {
        let state = lock(&self.state);
        json!({
            "lane": self.name,
            "pending": state.schedule.len(),
            "running": state.running,
            "last": state.last,
        })
    }

    /// Take the next due key and mark it running.
    fn take_due(&self, now: u64) -> Option<String> {
        let mut state = lock(&self.state);
        let key = state.schedule.pop_due(now)?;
        state.running = Some(key.clone());
        Some(key)
    }

    fn finish(&self, key: &str, result: Result<Outcome>, now: u64) {
        let mut state = lock(&self.state);
        state.running = None;
        match result {
            Ok(outcome) => {
                state.last.insert(key.to_owned(), outcome.summary);
                if let Some(seconds) = outcome.next_in {
                    state.schedule.at(key, now + seconds);
                }
            }
            Err(error) => {
                state
                    .last
                    .insert(key.to_owned(), json!({"error": error.to_string()}));
                state.schedule.at(key, now + ERROR_BACKOFF);
            }
        }
    }

    /// Run due passes until `stop` is set.
    pub fn work(&self, reconciler: &mut dyn Reconciler, stop: &AtomicBool) {
        while !stop.load(Ordering::SeqCst) {
            let current = now();
            if let Some(key) = self.take_due(current) {
                let result = reconciler.reconcile(&key);
                self.finish(&key, result, now());
                continue;
            }
            let state = lock(&self.state);
            let wait = state
                .schedule
                .next()
                .map_or(5, |when| when.saturating_sub(current).clamp(1, 5));
            drop(
                self.wake
                    .wait_timeout(state, Duration::from_secs(wait))
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
    }
}

// ------------------------------------------------------------------ service

/// What the receiver needs to answer deliveries.
pub struct Service {
    pub secret: Vec<u8>,
    pub debounce: u64,
    pub land: Arc<Lane>,
    /// Repositories whose landing queue is reconciled.
    pub land_repositories: BTreeSet<String>,
    pub mirror: Option<Arc<Lane>>,
    pub mirror_repositories: BTreeSet<String>,
    pub mirror_delay: u64,
    pub started: u64,
    /// Deliveries refused for a missing or wrong signature, and accepted ones.
    pub rejected: AtomicU64,
    pub accepted: AtomicU64,
}

impl Service {
    pub fn handle(&self, request: &Request, current: u64) -> Response {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/healthz") => Response::text(200, "ok"),
            ("GET", "/status") => {
                let mut lanes = vec![self.land.status()];
                lanes.extend(self.mirror.iter().map(|lane| lane.status()));
                Response::json(
                    200,
                    &json!({
                        "revision": crate::SOURCE_REVISION,
                        "started": self.started,
                        "deliveries": {
                            "accepted": self.accepted.load(Ordering::Relaxed),
                            "rejected": self.rejected.load(Ordering::Relaxed),
                        },
                        "lanes": lanes,
                    }),
                )
            }
            ("POST", WEBHOOK_PATH) => self.delivery(request, current),
            (_, "/healthz" | "/status" | WEBHOOK_PATH) => Response::text(405, "method not allowed"),
            _ => Response::text(404, "not found"),
        }
    }

    fn delivery(&self, request: &Request, current: u64) -> Response {
        let signature = request
            .header("x-forgejo-signature")
            .or_else(|| request.header("x-gitea-signature"))
            .or_else(|| request.header("x-hub-signature-256"));
        // Nothing is parsed or acted on before the signature has been verified.
        if !signature.is_some_and(|s| verify_signature(&self.secret, &request.body, s)) {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            return Response::text(401, "invalid signature");
        }
        let event = request
            .header("x-forgejo-event")
            .or_else(|| request.header("x-gitea-event"))
            .or_else(|| request.header("x-github-event"))
            .unwrap_or("");
        let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
            return Response::text(400, "invalid json");
        };
        let Some(delivery) = parse_delivery(event, &body) else {
            return Response::text(202, "ignored");
        };
        let mut marked = Vec::new();
        if self.land_repositories.contains(&delivery.repository) {
            self.land
                .mark(&delivery.repository, current + self.debounce);
            marked.push("land");
        }
        if let (
            Some(lane),
            Kind::Push {
                default_branch: true,
            },
        ) = (&self.mirror, &delivery.kind)
        {
            if self.mirror_repositories.contains(&delivery.repository) {
                lane.mark(&delivery.repository, current + self.mirror_delay);
                marked.push("mirror");
            }
        }
        self.accepted.fetch_add(1, Ordering::Relaxed);
        crate::event(
            json!({"event":"serve-delivery","kind":event,"repository":delivery.repository,"marked":marked}),
        );
        Response::json(
            202,
            &json!({"repository": delivery.repository, "marked": marked}),
        )
    }
}

/// Maximum simultaneous connections; further ones get 503.
const MAX_CONNECTIONS: usize = 32;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

fn connection(mut stream: TcpStream, service: &Service, limits: &Limits) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let response = match read_request(&mut stream, limits) {
        Ok(request) => service.handle(&request, now()),
        Err(reject) => Response::text(reject.status(), "rejected"),
    };
    let _ = write_response(&mut stream, &response);
}

/// Accept loop. Returns when `stop` is set.
pub fn serve(listener: &TcpListener, service: &Arc<Service>, stop: &AtomicBool) -> Result<()> {
    listener.set_nonblocking(true)?;
    let active = Arc::new(AtomicUsize::new(0));
    let limits = Limits::default();
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                    let _ = write_response(&mut stream, &Response::text(503, "busy"));
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let service = Arc::clone(service);
                let active = Arc::clone(&active);
                thread::spawn(move || {
                    connection(stream, &service, &limits);
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn hmac_matches_the_rfc_4231_vectors() {
        let cases: [(Vec<u8>, Vec<u8>, &str); 4] = [
            (
                vec![0x0b; 20],
                b"Hi There".to_vec(),
                "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
            ),
            (
                b"Jefe".to_vec(),
                b"what do ya want for nothing?".to_vec(),
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (
                vec![0xaa; 20],
                vec![0xdd; 50],
                "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
            ),
            (
                vec![0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
        ];
        for (key, data, expected) in cases {
            assert_eq!(hex(&hmac_sha256(&key, &data)), expected);
        }
    }

    #[test]
    fn signatures_are_verified_exactly() {
        let secret = b"s3cret";
        let body = br#"{"a":1}"#;
        let good = hex(&hmac_sha256(secret, body));
        assert!(verify_signature(secret, body, &good));
        assert!(verify_signature(secret, body, &format!("sha256={good}")));
        assert!(verify_signature(
            secret,
            body,
            &format!("  {}  ", good.to_uppercase())
        ));
        assert!(!verify_signature(secret, b"{\"a\":2}", &good));
        assert!(!verify_signature(b"other", body, &good));
        assert!(!verify_signature(secret, body, ""));
        assert!(!verify_signature(secret, body, "zz"));
        assert!(!verify_signature(secret, body, &good[..62]));
        assert!(!verify_signature(secret, body, &format!("{good}00")));
        assert!(constant_time_eq(b"ab", b"ab") && !constant_time_eq(b"ab", b"ac"));
        assert!(!constant_time_eq(b"ab", b"abc"));
    }

    fn parse(raw: &str) -> std::result::Result<Request, Reject> {
        read_request(
            &mut Cursor::new(raw.as_bytes().to_vec()),
            &Limits::default(),
        )
    }

    #[test]
    fn reads_a_well_formed_post_and_ignores_pipelined_bytes() {
        let request = parse("POST /hook?x=1 HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nX-Forgejo-Event: push\r\n\r\nhelloEXTRA").unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/hook");
        assert_eq!(request.body, b"hello");
        assert_eq!(request.header("x-forgejo-event"), Some("push"));
        assert!(parse("GET /healthz HTTP/1.1\r\n\r\n")
            .unwrap()
            .body
            .is_empty());
    }

    #[test]
    fn malformed_and_hostile_requests_are_rejected() {
        let big = format!(
            "POST /hook HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            5 * 1024 * 1024
        );
        for (raw, want) in [
            (
                "POST /hook HTTP/1.1\r\n\r\n".to_string(),
                Reject::LengthRequired,
            ),
            (
                "POST /hook HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".into(),
                Reject::NotImplemented,
            ),
            (
                "POST /hook HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab".into(),
                Reject::BadRequest,
            ),
            (
                "POST /hook HTTP/1.1\r\nContent-Length: -1\r\n\r\n".into(),
                Reject::BadRequest,
            ),
            (
                "POST /hook HTTP/1.1\r\nContent-Length: 10\r\n\r\nshort".into(),
                Reject::BadRequest,
            ),
            (
                "GET /x HTTP/1.1\r\nBad Header: v\r\n\r\n".into(),
                Reject::BadRequest,
            ),
            (
                "GET /x HTTP/1.1\r\nA: b\r\n folded\r\n\r\n".into(),
                Reject::BadRequest,
            ),
            (
                "GET /x HTTP/1.1\r\nnocolon\r\n\r\n".into(),
                Reject::BadRequest,
            ),
            ("GET x HTTP/1.1\r\n\r\n".into(), Reject::BadRequest),
            ("get /x HTTP/1.1\r\n\r\n".into(), Reject::BadRequest),
            ("GET /x SPDY/3\r\n\r\n".into(), Reject::BadRequest),
            ("GET /x HTTP/1.1 extra\r\n\r\n".into(), Reject::BadRequest),
            (big, Reject::BodyTooLarge),
            (
                format!("GET /x HTTP/1.1\r\nA: {}\r\n\r\n", "x".repeat(20_000)),
                Reject::HeadersTooLarge,
            ),
        ] {
            assert_eq!(
                parse(&raw).unwrap_err(),
                want,
                "{}",
                &raw[..raw.len().min(60)]
            );
        }
        let many = format!(
            "GET /x HTTP/1.1\r\n{}\r\n",
            (0..70).map(|i| format!("H{i}: v\r\n")).collect::<String>()
        );
        assert_eq!(parse(&many).unwrap_err(), Reject::HeadersTooLarge);
    }

    #[test]
    fn deliveries_are_routed_by_event_and_repository() {
        let push = json!({"ref":"refs/heads/main","repository":{"full_name":"o/r","default_branch":"main"}});
        assert_eq!(
            parse_delivery("push", &push),
            Some(Delivery {
                repository: "o/r".into(),
                kind: Kind::Push {
                    default_branch: true
                }
            })
        );
        let other = json!({"ref":"refs/heads/ci/x","repository":{"full_name":"o/r","default_branch":"main"}});
        assert_eq!(
            parse_delivery("push", &other).unwrap().kind,
            Kind::Push {
                default_branch: false
            }
        );
        let pr = json!({"action":"opened","repository":{"full_name":"o/r"}});
        assert_eq!(
            parse_delivery("pull_request", &pr).unwrap().kind,
            Kind::PullRequest
        );
        assert_eq!(
            parse_delivery("pull_request_sync", &pr).unwrap().kind,
            Kind::PullRequest
        );
        assert!(parse_delivery("release", &pr).is_none());
        assert!(parse_delivery("push", &json!({"repository":{"full_name":"o/r"}})).is_none());
        assert!(
            parse_delivery("pull_request", &json!({"repository":{"full_name":"../x"}})).is_none()
        );
        assert!(parse_delivery("pull_request", &json!({})).is_none());
    }

    #[test]
    fn schedule_events_bring_work_forward_never_back() {
        let mut schedule = Schedule::default();
        schedule.at("a", 100);
        schedule.at("a", 50);
        schedule.at("a", 200);
        schedule.at("b", 60);
        assert_eq!(schedule.next(), Some(50));
        assert_eq!(schedule.pop_due(49), None);
        assert_eq!(schedule.pop_due(70).as_deref(), Some("a"));
        assert_eq!(schedule.pop_due(70).as_deref(), Some("b"));
        assert!(schedule.is_empty());
    }

    struct Scripted(Vec<Result<Outcome>>, Vec<String>);
    impl Reconciler for Scripted {
        fn reconcile(&mut self, key: &str) -> Result<Outcome> {
            self.1.push(key.to_owned());
            self.0.remove(0)
        }
    }

    #[test]
    fn a_lane_rearms_by_outcome_and_backs_off_after_errors() {
        let lane = Lane::new("test");
        lane.mark("o/r", 10);
        assert_eq!(lane.take_due(9), None);
        let key = lane.take_due(10).unwrap();
        // An event arriving while the pass runs is kept.
        lane.mark("o/r", 12);
        lane.finish(
            &key,
            Ok(Outcome {
                summary: json!({"waiting": true}),
                next_in: Some(30),
            }),
            11,
        );
        assert_eq!(lane.take_due(12).as_deref(), Some("o/r"));
        lane.finish("o/r", Err(failure("boom")), 12);
        assert_eq!(lane.take_due(71), None);
        assert_eq!(lane.take_due(72).as_deref(), Some("o/r"));
        lane.finish(
            "o/r",
            Ok(Outcome {
                summary: json!({}),
                next_in: None,
            }),
            72,
        );
        assert_eq!(lane.take_due(10_000), None);
        let status = lane.status();
        assert_eq!(status["pending"], 0);
        assert_eq!(status["running"], Value::Null);
        // The worker loop runs a due key and stops on request.
        let lane = Lane::new("loop");
        lane.mark("k", 0);
        let stop = AtomicBool::new(false);
        let mut scripted = Scripted(
            vec![Ok(Outcome {
                summary: json!({"done": true}),
                next_in: None,
            })],
            Vec::new(),
        );
        thread::scope(|scope| {
            scope.spawn(|| lane.work(&mut scripted, &stop));
            while lane.status()["last"]["k"].is_null() {
                thread::sleep(Duration::from_millis(20));
            }
            stop.store(true, Ordering::SeqCst);
            lane.wake.notify_all();
        });
        assert_eq!(scripted.1, ["k"]);
    }

    fn service() -> (Service, Arc<Lane>, Arc<Lane>) {
        let land = Lane::new("land");
        let mirror = Lane::new("mirror");
        let service = Service {
            secret: b"topsecret".to_vec(),
            debounce: 3,
            land: land.clone(),
            land_repositories: BTreeSet::from(["o/r".to_string()]),
            mirror: Some(mirror.clone()),
            mirror_repositories: BTreeSet::from(["o/r".to_string(), "o/m".to_string()]),
            mirror_delay: 60,
            started: 1,
            rejected: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
        };
        (service, land, mirror)
    }

    fn signed(service: &Service, event: &str, body: &Value) -> Request {
        let raw = body.to_string().into_bytes();
        let mut headers = BTreeMap::new();
        headers.insert(
            "x-forgejo-signature".into(),
            hex(&hmac_sha256(&service.secret, &raw)),
        );
        headers.insert("x-forgejo-event".into(), event.into());
        Request {
            method: "POST".into(),
            path: "/hook".into(),
            headers,
            body: raw,
        }
    }

    #[test]
    fn only_signed_deliveries_mark_declared_repositories() {
        let (service, land, mirror) = service();
        let push = json!({"ref":"refs/heads/main","repository":{"full_name":"o/r","default_branch":"main"}});
        // Unsigned, wrongly signed and unsigned-by-header deliveries do nothing.
        let mut bad = signed(&service, "push", &push);
        bad.headers
            .insert("x-forgejo-signature".into(), "00".repeat(32));
        assert_eq!(service.handle(&bad, 100).status, 401);
        let mut none = signed(&service, "push", &push);
        none.headers.remove("x-forgejo-signature");
        assert_eq!(service.handle(&none, 100).status, 401);
        assert!(land.take_due(u64::MAX).is_none());
        // A signed push to the default branch marks both lanes with their delays.
        let response = service.handle(&signed(&service, "push", &push), 100);
        assert_eq!(response.status, 202);
        assert!(land.take_due(102).is_none());
        assert_eq!(land.take_due(103).as_deref(), Some("o/r"));
        assert!(mirror.take_due(159).is_none());
        assert_eq!(mirror.take_due(160).as_deref(), Some("o/r"));
        // Pull request events mark only landing; undeclared repositories nothing.
        let pr = json!({"action":"opened","repository":{"full_name":"o/r"}});
        assert_eq!(
            service
                .handle(&signed(&service, "pull_request", &pr), 200)
                .status,
            202
        );
        assert!(land.take_due(203).is_some() && mirror.take_due(u64::MAX).is_none());
        let stranger = json!({"action":"opened","repository":{"full_name":"x/y"}});
        assert_eq!(
            service
                .handle(&signed(&service, "pull_request", &stranger), 300)
                .status,
            202
        );
        assert!(land.take_due(u64::MAX).is_none());
        // Mirror-only repository: push marks the mirror lane, not landing.
        let m = json!({"ref":"refs/heads/main","repository":{"full_name":"o/m","default_branch":"main"}});
        service.handle(&signed(&service, "push", &m), 400);
        assert!(land.take_due(u64::MAX).is_none() && mirror.take_due(u64::MAX).is_some());
        // Valid signature, broken JSON.
        let mut broken = signed(&service, "push", &push);
        broken.body = b"{".to_vec();
        broken.headers.insert(
            "x-forgejo-signature".into(),
            hex(&hmac_sha256(&service.secret, b"{")),
        );
        assert_eq!(service.handle(&broken, 500).status, 400);
        // Other paths and methods.
        let get = |path: &str, method: &str| Request {
            method: method.into(),
            path: path.into(),
            headers: BTreeMap::new(),
            body: Vec::new(),
        };
        assert_eq!(service.handle(&get("/healthz", "GET"), 1).status, 200);
        assert_eq!(service.handle(&get("/status", "GET"), 1).status, 200);
        assert_eq!(service.handle(&get("/hook", "GET"), 1).status, 405);
        assert_eq!(service.rejected.load(Ordering::Relaxed), 2);
        assert_eq!(service.accepted.load(Ordering::Relaxed), 4);
        assert_eq!(service.handle(&get("/other", "GET"), 1).status, 404);
    }

    #[test]
    fn serves_over_a_real_socket_and_stops() {
        let (service, land, _) = service();
        let service = Arc::new(service);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let (service, stop) = (service.clone(), stop.clone());
            thread::spawn(move || serve(&listener, &service, &stop).unwrap())
        };
        let exchange = |raw: Vec<u8>| -> String {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream.write_all(&raw).unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).unwrap();
            answer
        };
        assert!(exchange(b"GET /healthz HTTP/1.1\r\n\r\n".to_vec()).starts_with("HTTP/1.1 200 OK"));
        let body = json!({"action":"opened","repository":{"full_name":"o/r"}}).to_string();
        let sig = hex(&hmac_sha256(&service.secret, body.as_bytes()));
        let raw = format!(
            "POST /hook HTTP/1.1\r\nHost: x\r\nX-Forgejo-Event: pull_request\r\nX-Forgejo-Signature: {sig}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        assert!(exchange(raw.into_bytes()).starts_with("HTTP/1.1 202 Accepted"));
        assert!(land.take_due(u64::MAX).is_some());
        let forged = format!(
            "POST /hook HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        assert!(exchange(forged.into_bytes()).starts_with("HTTP/1.1 401"));
        assert!(exchange(b"nonsense\r\n\r\n".to_vec()).starts_with("HTTP/1.1 400"));
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
    }

    #[test]
    fn serve_policy_is_validated() {
        let ok = |v: Value| serde_json::from_value::<ServePolicy>(v).unwrap().validate();
        let base = json!({"listen":"0.0.0.0:8080","secret_env":"CFRG_WEBHOOK_SECRET"});
        assert!(ok(base.clone()).is_ok());
        let mut with = base.clone();
        with["webhook"] = json!({"url":"http://cfrg-serve.ci.svc.cluster.local:8080/hook","orgs":["corbet-libs"]});
        with["mirrors"] = json!({"placement":"/p.json","state":"/s.json"});
        assert!(ok(with).is_ok());
        for bad in [
            json!({"listen":"nonsense","secret_env":"X"}),
            json!({"listen":"0.0.0.0:8080","secret_env":"lower"}),
            json!({"listen":"0.0.0.0:8080","secret_env":"X","sweep_seconds":5}),
            json!({"listen":"0.0.0.0:8080","secret_env":"X","webhook":{"url":"http://u:p@h/hook"}}),
            json!({"listen":"0.0.0.0:8080","secret_env":"X","webhook":{"url":"ftp://h/hook"}}),
            json!({"listen":"0.0.0.0:8080","secret_env":"X","webhook":{"url":"http://h/other"}}),
            json!({"listen":"0.0.0.0:8080","secret_env":"X","webhook":{"url":"http://h/hook","orgs":["a/b"]}}),
            json!({"listen":"0.0.0.0:8080","secret_env":"X","mirrors":{"placement":"","state":"/s"}}),
        ] {
            assert!(ok(bad.clone()).is_err(), "{bad}");
        }
    }
}
