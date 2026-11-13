//! One provider endpoint: a base URL, fixed headers, counted writes, an optional pacer before each write.
//!
//! Redirects are followed by hand so that a redirect to another host (a provider's object store) never carries our
//! credential. A rate limit is an error, not a retry: the caller stops the copy.
use reqwest::{header::HeaderMap, header::HeaderName, header::HeaderValue, Body, Client, Method};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, sync::Mutex};

pub const USER_AGENT: &str = "corbet-forge-bridge/1";
const MAX_REDIRECTS: usize = 5;

#[derive(Debug)]
pub enum ApiError {
    RateLimited(String),
    Io(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited(m) | Self::Io(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ApiError {}

fn io(error: impl std::fmt::Display) -> ApiError {
    ApiError::Io(error.to_string())
}

pub struct Pacer {
    min: Duration,
    last: Mutex<Option<Instant>>,
}

impl Pacer {
    pub fn new(min_seconds: f64) -> Self {
        Self {
            min: Duration::from_secs_f64(min_seconds.max(0.0)),
            last: Mutex::new(None),
        }
    }

    async fn wait(&self) {
        let mut last = self.last.lock().await;
        if let Some(at) = *last {
            let due = at + self.min;
            if let Some(pause) = due.checked_duration_since(Instant::now()) {
                tokio::time::sleep(pause).await;
            }
        }
        *last = Some(Instant::now());
    }
}

pub struct Response {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: bytes::Bytes,
}

pub enum Payload<'a> {
    None,
    Json(&'a Value),
    /// A file streamed as the body, with its length.
    File(&'a Path, u64),
}

pub struct Api {
    base: String,
    headers: HeaderMap,
    client: Client,
    pacer: Option<Arc<Pacer>>,
    writes: AtomicU64,
}

impl Api {
    pub fn new(
        client: Client,
        base: &str,
        extra: &[(&str, &str)],
        pacer: Option<Arc<Pacer>>,
    ) -> Result<Self, ApiError> {
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", HeaderValue::from_static(USER_AGENT));
        for (name, value) in extra {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).map_err(io)?,
                HeaderValue::from_str(value).map_err(io)?,
            );
        }
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            headers,
            client,
            pacer,
            writes: AtomicU64::new(0),
        })
    }

    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    pub fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_owned()
        } else {
            format!("{}{path}", self.base)
        }
    }

    async fn open(
        &self,
        method: Method,
        path: &str,
        payload: Payload<'_>,
        extra: &[(&str, &str)],
    ) -> Result<reqwest::Response, ApiError> {
        let write = method != Method::GET && method != Method::HEAD;
        if write {
            if let Some(pacer) = &self.pacer {
                pacer.wait().await;
            }
            self.writes.fetch_add(1, Ordering::Relaxed);
        }
        let mut url = self.url(path);
        let mut headers = self.headers.clone();
        for (name, value) in extra {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).map_err(io)?,
                HeaderValue::from_str(value).map_err(io)?,
            );
        }
        let mut hops = 0;
        loop {
            let mut request = self
                .client
                .request(method.clone(), &url)
                .headers(headers.clone());
            match &payload {
                Payload::None => {}
                Payload::Json(value) => request = request.json(value),
                Payload::File(file, length) => {
                    let handle = tokio::fs::File::open(file).await.map_err(io)?;
                    let stream = tokio_util::io::ReaderStream::with_capacity(handle, 1 << 20);
                    request = request
                        .header("content-length", length.to_string())
                        .body(Body::wrap_stream(stream));
                }
            }
            let response = request.send().await.map_err(io)?;
            let redirect = matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308);
            if !redirect
                || !(method == Method::GET || method == Method::HEAD)
                || hops >= MAX_REDIRECTS
            {
                return Ok(response);
            }
            let Some(location) = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
            else {
                return Ok(response);
            };
            let next = response.url().join(location).map_err(io)?;
            if next.host_str() != response.url().host_str()
                || next.port_or_known_default() != response.url().port_or_known_default()
            {
                headers.remove("authorization");
                headers.remove("private-token");
            }
            url = next.to_string();
            hops += 1;
        }
    }

    /// -> (status, headers, bytes). Never an error on an HTTP status, except a rate limit.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        payload: Payload<'_>,
        extra: &[(&str, &str)],
    ) -> Result<Response, ApiError> {
        let label = format!("{method} {}", path.split('?').next().unwrap_or(path));
        let response = self.open(method, path, payload, extra).await?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.bytes().await.map_err(io)?;
        let head = &body[..body.len().min(2000)];
        if status == 429
            || (status == 403
                && (headers.contains_key("retry-after")
                    || String::from_utf8_lossy(head)
                        .to_lowercase()
                        .contains("rate limit")))
        {
            return Err(ApiError::RateLimited(format!("{label} -> {status}")));
        }
        Ok(Response {
            status,
            headers,
            body,
        })
    }

    pub async fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Option<Value>), ApiError> {
        let payload = body.map_or(Payload::None, Payload::Json);
        let response = self.call(method, path, payload, &[]).await?;
        let value = if response.body.is_empty() {
            None
        } else {
            serde_json::from_slice(&response.body).ok()
        };
        Ok((response.status, value))
    }

    pub async fn get_json(&self, path: &str) -> Result<(u16, Option<Value>), ApiError> {
        self.json(Method::GET, path, None).await
    }

    /// Stream a GET to `dest` (or nowhere); -> (status, size, sha256 hex). A non-200 returns (status, 0, "").
    pub async fn download(
        &self,
        path: &str,
        dest: Option<&Path>,
        extra: &[(&str, &str)],
    ) -> Result<(u16, u64, String), ApiError> {
        let mut response = self.open(Method::GET, path, Payload::None, extra).await?;
        let status = response.status().as_u16();
        if status != 200 {
            return Ok((status, 0, String::new()));
        }
        let mut digest = Sha256::new();
        let mut size = 0u64;
        let mut out = match dest {
            Some(path) => Some(tokio::fs::File::create(path).await.map_err(io)?),
            None => None,
        };
        while let Some(block) = response.chunk().await.map_err(io)? {
            digest.update(&block);
            size += block.len() as u64;
            if let Some(file) = out.as_mut() {
                file.write_all(&block).await.map_err(io)?;
            }
        }
        if let Some(mut file) = out {
            file.flush().await.map_err(io)?;
        }
        Ok((200, size, hex::encode(digest.finalize())))
    }
}
