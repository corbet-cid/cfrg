//! The service: ONE process, TWO listeners, one verifier per forge, one worker for release uploads and one for
//! dispatches.
//!
//! ```text
//! internal (TLS 1.3, cluster CA)  Forgejo and internal callers
//!   POST /hooks/release   key hmac_forgejo   release upload (R45)
//!   POST /hooks/sync      key hmac_forgejo   a tag pusher's hint "OWNER/REPO TAG is on a copy now"
//!   POST /hooks/forgejo   key hmac_forgejo   issues, comments, pull requests of the primary -> relay.yml
//!   GET  /healthz
//! public (plain HTTP behind the tunnel)  the providers
//!   POST /hooks/github    key hmac_github    -> relay.yml
//!   POST /hooks/gitlab    key hmac_gitlab    -> relay.yml (Standard Webhooks)
//!   GET  /healthz
//! ```
//!
//! A listener answers nothing that is not in its rows. A request carries no authority: it names a repository and a
//! tag or an object number, and the work re-reads everything from the forge. The signature keeps strangers from
//! spending our rate limits and starting jobs; it is not what makes the work safe.
use crate::{
    config::{Config, Credentials},
    dispatch::Dispatcher,
    event::{self, Dispatch},
    release::{self, Ctx},
    verify::{self, Reject, Replay},
};
use bytes::Bytes;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    sync::{mpsc, watch},
};
use tokio_rustls::TlsAcceptor;

pub const MAX_BODY: usize = 1 << 20;

pub fn log(event: &str, fields: Value) {
    let mut line = json!({"service": "cfrg-serve", "event": event});
    if let (Some(line), Some(extra)) = (line.as_object_mut(), fields.as_object()) {
        for (k, v) in extra {
            line.insert(k.clone(), v.clone());
        }
    }
    println!("{line}");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Release,
    Sync,
    Forgejo,
    Github,
    Gitlab,
}

/// (listener, path) -> route. A listener answers nothing that is not in its rows.
fn route(listener: &str, path: &str) -> Option<Route> {
    match (listener, path) {
        ("internal", "/hooks/release") => Some(Route::Release),
        ("internal", "/hooks/sync") => Some(Route::Sync),
        ("internal", "/hooks/forgejo") => Some(Route::Forgejo),
        ("public", "/hooks/github") => Some(Route::Github),
        ("public", "/hooks/gitlab") => Some(Route::Gitlab),
        _ => None,
    }
}

struct DispatchJob {
    dispatch: Dispatch,
    replay_key: Option<String>,
}

pub struct Bridge {
    ctx: Arc<Ctx>,
    replay: Replay,
    dispatcher: Option<Dispatcher>,
    release_tx: mpsc::Sender<(String, String)>,
    dispatch_tx: mpsc::Sender<DispatchJob>,
    wiki_tx: mpsc::Sender<String>,
    wiki_pending: Mutex<HashSet<String>>,
    pending: Mutex<HashSet<(String, String)>>,
    last: Mutex<HashMap<(String, String), i32>>,
}

/// The receiving halves, handed to [`Bridge::run_workers`].
pub struct Workers {
    release_rx: mpsc::Receiver<(String, String)>,
    dispatch_rx: mpsc::Receiver<DispatchJob>,
    wiki_rx: mpsc::Receiver<String>,
}

impl Bridge {
    pub fn new(cfg: Config, creds: Credentials) -> crate::config::Result<(Arc<Self>, Workers)> {
        let replay = Replay::new(
            Duration::from_secs(cfg.replay.ttl_seconds),
            cfg.replay.capacity,
        );
        let dispatcher = match (&cfg.dispatch, creds.get("dispatch_token")) {
            (Some(target), Some(token)) => Some(Dispatcher::new(
                crate::http::forge_client(cfg.forge_ca.as_deref().map(Path::new))?,
                &cfg.forge_api,
                target,
                token,
            )),
            _ => None,
        };
        let ctx = Arc::new(Ctx::new(cfg, creds)?);
        let (release_tx, release_rx) = mpsc::channel(64);
        let (dispatch_tx, dispatch_rx) = mpsc::channel(256);
        let (wiki_tx, wiki_rx) = mpsc::channel(64);
        let bridge = Arc::new(Self {
            ctx,
            replay,
            dispatcher,
            release_tx,
            dispatch_tx,
            wiki_tx,
            wiki_pending: Mutex::new(HashSet::new()),
            pending: Mutex::new(HashSet::new()),
            last: Mutex::new(HashMap::new()),
        });
        Ok((
            bridge,
            Workers {
                release_rx,
                dispatch_rx,
                wiki_rx,
            },
        ))
    }

    pub fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    /// The exit status of the last finished sync of this repository and tag.
    pub fn last(&self, repo: &str, tag: &str) -> Option<i32> {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(repo.to_owned(), tag.to_owned()))
            .copied()
    }

    /// The two workers, each its own task: a long upload never delays a dispatch.
    pub fn run_workers(self: &Arc<Self>, workers: Workers) {
        let Workers {
            mut release_rx,
            mut dispatch_rx,
            mut wiki_rx,
        } = workers;
        let bridge = Arc::clone(self);
        tokio::spawn(async move {
            while let Some((repo, tag)) = release_rx.recv().await {
                // A hint that arrives while this runs queues one more pass, which is a no-op when nothing changed.
                bridge
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&(repo.clone(), tag.clone()));
                let exit = match &bridge.ctx.cfg.release_command {
                    Some(argv) if !argv.is_empty() => tokio::process::Command::new(&argv[0])
                        .args(&argv[1..])
                        .arg(&repo)
                        .arg(&tag)
                        .status()
                        .await
                        .ok()
                        .and_then(|s| s.code())
                        .unwrap_or(1),
                    _ => release::sync(&bridge.ctx, &repo, &tag, false).await.exit,
                };
                bridge
                    .last
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert((repo.clone(), tag.clone()), exit);
                log(
                    "done",
                    json!({"kind": "release", "repo": repo, "tag": tag, "exit": exit}),
                );
            }
        });
        let bridge = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(repository) = wiki_rx.recv().await {
                // A hook that arrives while this runs queues one more pass, which is a no-op when nothing changed.
                bridge
                    .wiki_pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&repository);
                let worker = Arc::clone(&bridge);
                let id = repository.clone();
                let result = tokio::task::spawn_blocking(move || {
                    crate::wiki::sync(&worker.ctx.cfg, &worker.ctx.creds, &id)
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r);
                match result {
                    Ok(report) => log(
                        "done",
                        json!({"kind": "wiki", "repository": repository, "report": report}),
                    ),
                    Err(error) => log(
                        "failed",
                        json!({"kind": "wiki", "repository": repository, "error": error}),
                    ),
                }
            }
        });
        let bridge = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(job) = dispatch_rx.recv().await {
                let Some(dispatcher) = &bridge.dispatcher else {
                    continue;
                };
                match dispatcher.run(&job.dispatch).await {
                    Ok(()) => log(
                        "dispatched",
                        json!({"forge": job.dispatch.forge.as_str(), "kind": job.dispatch.kind, "number": job.dispatch.number}),
                    ),
                    Err(error) => {
                        // One attempt (R30): free the delivery id so the sender's redelivery is accepted.
                        if let Some(key) = &job.replay_key {
                            bridge.replay.release(key);
                        }
                        log(
                            "dispatch-failed",
                            json!({"forge": job.dispatch.forge.as_str(), "error": error}),
                        );
                    }
                }
            }
        });
    }

    fn enqueue_release(&self, repo: &str, tag: &str) -> &'static str {
        let key = (repo.to_owned(), tag.to_owned());
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if !pending.insert(key.clone()) {
                return "duplicate";
            }
        }
        if self.release_tx.try_send(key.clone()).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            return "busy";
        }
        "queued"
    }
}

type Reply = Response<Full<Bytes>>;

fn reply(code: u16, text: &str) -> Reply {
    let mut response = Response::new(Full::new(Bytes::from(format!("{text}\n"))));
    *response.status_mut() =
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response.headers_mut().insert(
        "content-type",
        http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn verified(
    route: Route,
    creds: &Credentials,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Option<String>, Reject> {
    match route {
        Route::Release | Route::Sync | Route::Forgejo => {
            verify::forgejo(creds.get("hmac_forgejo"), headers, body)
        }
        Route::Github => verify::github(creds.get("hmac_github"), headers, body),
        Route::Gitlab => verify::gitlab(creds.get("hmac_gitlab"), headers, body, now_unix()),
    }
}

/// -> (repo, tag) of a published, non-draft release event.
fn release_request(payload: &Value) -> Option<(String, String)> {
    if payload["action"].as_str() != Some("published")
        || payload["release"]["draft"].as_bool() == Some(true)
    {
        return None;
    }
    let repo = payload["repository"]["full_name"]
        .as_str()
        .filter(|s| !s.is_empty())?;
    let tag = payload["release"]["tag_name"]
        .as_str()
        .filter(|s| !s.is_empty())?;
    Some((repo.to_owned(), tag.to_owned()))
}

/// The hint of a tag pusher: `{"repository": "OWNER/REPO", "tag": "vX"}`.
fn sync_request(payload: &Value) -> Option<(String, String)> {
    Some((
        payload["repository"].as_str()?.to_owned(),
        payload["tag"].as_str()?.to_owned(),
    ))
}

pub async fn handle(bridge: Arc<Bridge>, listener: Arc<str>, request: Request<Incoming>) -> Reply {
    let (parts, body) = request.into_parts();
    if parts.method == Method::GET && parts.uri.path() == "/healthz" {
        return reply(200, "ok");
    }
    let path = parts.uri.path();
    let Some(route) = route(&listener, path).filter(|_| parts.method == Method::POST) else {
        return reply(404, "not found");
    };
    match parts
        .headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .map(str::parse::<usize>)
    {
        Some(Ok(length)) if length <= MAX_BODY => {}
        Some(Ok(_)) => return reply(413, "bad length"),
        _ => return reply(411, "bad length"),
    }
    let Ok(collected) = Limited::new(body, MAX_BODY).collect().await else {
        return reply(413, "bad length");
    };
    let body = collected.to_bytes();
    let delivery = match verified(route, &bridge.ctx.creds, &parts.headers, &body) {
        Ok(id) => id,
        Err(reason) => {
            log(
                "refused",
                json!({"listener": &*listener, "path": path, "reason": reason.reason()}),
            );
            return reply(401, "bad signature");
        }
    };
    // Replay protection by delivery id; a sender that gives none (the tag pusher) is idempotent by construction.
    let replay_key = delivery.map(|id| format!("{route:?}/{id}"));
    if let Some(key) = &replay_key {
        if !bridge.replay.reserve(key, Instant::now()) {
            log("replayed", json!({"listener": &*listener, "path": path}));
            return reply(200, "duplicate");
        }
    }
    let release_key = |key: &Option<String>| {
        if let Some(key) = key {
            bridge.replay.release(key);
        }
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        release_key(&replay_key);
        return reply(400, "bad json");
    };
    match route {
        Route::Release | Route::Sync => {
            let request = if route == Route::Release {
                release_request(&payload)
            } else {
                sync_request(&payload)
            };
            // Another action or event: nothing to do, and the sender must not retry.
            let Some((repo, tag)) = request else {
                return reply(200, "ignored");
            };
            if !release::valid_repo(&repo) || !release::valid_tag(&tag) {
                log(
                    "refused",
                    json!({"listener": &*listener, "path": path, "reason": "shape"}),
                );
                return reply(400, "bad repository or tag");
            }
            let state = bridge.enqueue_release(&repo, &tag);
            log(
                "accepted",
                json!({"listener": &*listener, "kind": "release", "repo": repo, "tag": tag, "state": state}),
            );
            if state == "busy" {
                release_key(&replay_key);
                return reply(503, "busy");
            }
            reply(202, state)
        }
        Route::Forgejo | Route::Github | Route::Gitlab => {
            let dispatch = match route {
                Route::Forgejo => event::forgejo(&parts.headers, &payload),
                Route::Github => event::github(&parts.headers, &payload),
                _ => event::gitlab(&parts.headers, &payload),
            };
            let Some(dispatch) = dispatch else {
                return reply(200, "ignored");
            };
            if dispatch.kind == "wiki" {
                if bridge.ctx.cfg.wiki.is_none() {
                    release_key(&replay_key);
                    return reply(503, "the wiki leg is not configured");
                }
                let first = bridge
                    .wiki_pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(dispatch.repository.clone());
                if !first {
                    return reply(202, "duplicate");
                }
                if bridge
                    .wiki_tx
                    .try_send(dispatch.repository.clone())
                    .is_err()
                {
                    bridge
                        .wiki_pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&dispatch.repository);
                    release_key(&replay_key);
                    return reply(503, "busy");
                }
                log(
                    "accepted",
                    json!({"listener": &*listener, "kind": "wiki", "repository": dispatch.repository}),
                );
                return reply(202, "queued");
            }
            if bridge.dispatcher.is_none() {
                release_key(&replay_key);
                log(
                    "refused",
                    json!({"listener": &*listener, "path": path, "reason": "no dispatch target or token"}),
                );
                return reply(503, "dispatch is not configured");
            }
            let job = DispatchJob {
                dispatch,
                replay_key: replay_key.clone(),
            };
            let (forge, kind, number) = (
                job.dispatch.forge.as_str(),
                job.dispatch.kind,
                job.dispatch.number,
            );
            if bridge.dispatch_tx.try_send(job).is_err() {
                release_key(&replay_key);
                return reply(503, "busy");
            }
            log(
                "accepted",
                json!({"listener": &*listener, "kind": "dispatch", "forge": forge, "object": kind, "number": number}),
            );
            reply(202, "queued")
        }
    }
}

/// TLS 1.3 only, from `tls.crt` and `tls.key` in `dir` (read once at start; Reloader restarts the pod on renewal).
pub fn tls_acceptor(dir: &Path) -> crate::config::Result<TlsAcceptor> {
    use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let certificates =
        CertificateDer::pem_file_iter(dir.join("tls.crt"))?.collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_file(dir.join("tls.key"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(certificates, key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn connection<S>(bridge: Arc<Bridge>, listener: Arc<str>, stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |request| {
        let (bridge, listener) = (Arc::clone(&bridge), Arc::clone(&listener));
        async move { Ok::<_, Infallible>(handle(bridge, listener, request).await) }
    });
    let served = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(30))
        .serve_connection(TokioIo::new(stream), service)
        .await;
    if let Err(error) = served {
        log("connection-error", json!({"error": error.to_string()}));
    }
}

/// Accept connections on `socket` until `stop` flips. A failed handshake is one log line.
pub async fn serve(
    bridge: Arc<Bridge>,
    name: String,
    socket: TcpListener,
    tls: Option<TlsAcceptor>,
    mut stop: watch::Receiver<bool>,
) {
    let name: Arc<str> = Arc::from(name.as_str());
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            accepted = socket.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let (bridge, name, tls) = (Arc::clone(&bridge), Arc::clone(&name), tls.clone());
                tokio::spawn(async move {
                    match tls {
                        Some(acceptor) => {
                            match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream)).await {
                                Ok(Ok(secured)) => connection(bridge, name, secured).await,
                                Ok(Err(error)) => log("connection-error", json!({"listener": &*name, "error": error.to_string()})),
                                Err(_) => log("connection-error", json!({"listener": &*name, "error": "handshake timeout"})),
                            }
                        }
                        None => connection(bridge, name, stream).await,
                    }
                });
            }
        }
    }
}
