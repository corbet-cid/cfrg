//! Loopback fakes of Forgejo, GitHub, GitLab and an object store, the parity fixture of the Python bridge's tests.
#![allow(dead_code)]
use cfrg_serve::{
    config::{Config, Credentials},
    release::Ctx,
};
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    convert::Infallible,
    sync::{Arc, Mutex},
};

pub const COMMIT: &str = "481c2ee199b40870dbceb09ba96260af73d726fd";

pub fn sha(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }
}

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub fn json_resp(status: u16, value: Value) -> Resp {
    Resp {
        status,
        headers: vec![],
        body: value.to_string().into_bytes(),
    }
}

pub fn bytes_resp(status: u16, body: Vec<u8>) -> Resp {
    Resp {
        status,
        headers: vec![],
        body,
    }
}

pub struct Fake {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Req>>>,
}

impl Fake {
    pub async fn start(handler: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> Fake {
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", socket.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let log = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = socket.accept().await else {
                    continue;
                };
                let (handler, log) = (handler.clone(), log.clone());
                tokio::spawn(async move {
                    let service = service_fn(move |request: http::Request<Incoming>| {
                        let (handler, log) = (handler.clone(), log.clone());
                        async move {
                            let (parts, body) = request.into_parts();
                            let body = body
                                .collect()
                                .await
                                .map(|b| b.to_bytes().to_vec())
                                .unwrap_or_default();
                            let query = parts
                                .uri
                                .query()
                                .unwrap_or("")
                                .split('&')
                                .filter(|p| !p.is_empty())
                                .map(|p| {
                                    let (k, v) = p.split_once('=').unwrap_or((p, ""));
                                    (unquote(k), unquote(v))
                                })
                                .collect();
                            let req = Req {
                                method: parts.method.to_string(),
                                path: parts.uri.path().to_owned(),
                                query,
                                headers: parts.headers,
                                body,
                            };
                            log.lock().unwrap().push(req.clone());
                            let resp = handler(&req);
                            let mut out =
                                http::Response::new(Full::new(bytes::Bytes::from(resp.body)));
                            *out.status_mut() = http::StatusCode::from_u16(resp.status).unwrap();
                            for (k, v) in resp.headers {
                                out.headers_mut().insert(
                                    http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                                    v.parse().unwrap(),
                                );
                            }
                            Ok::<_, Infallible>(out)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Fake { url, requests }
    }

    pub fn all(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }

    pub fn writes(&self) -> Vec<Req> {
        self.all()
            .into_iter()
            .filter(|r| r.method != "GET" && r.method != "HEAD")
            .collect()
    }
}

#[derive(Clone)]
pub struct GAsset {
    pub id: u64,
    pub name: String,
    pub state: String,
    pub size: u64,
    pub digest: Option<String>,
    pub data: Vec<u8>,
}

impl GAsset {
    fn json(&self) -> Value {
        json!({"id": self.id, "name": self.name, "state": self.state, "size": self.size, "digest": self.digest})
    }
}

pub struct State {
    pub tag: String,
    pub files: BTreeMap<String, Vec<u8>>,
    pub served: BTreeMap<String, Vec<u8>>,
    pub mirror: bool,
    pub draft: bool,
    pub g_tags: HashMap<String, String>,
    pub g_actions: bool,
    pub g_releases: Vec<Value>,
    pub g_assets: HashMap<u64, BTreeMap<String, GAsset>>,
    pub g_limit: bool,
    pub g_org_page: u16,
    pub g_next: u64,
    pub g_truncate: bool,
    pub l_tags: HashMap<String, String>,
    pub l_builds: String,
    pub l_files: BTreeMap<String, Vec<Vec<u8>>>,
    pub l_release: Option<Value>,
    pub l_links: Vec<Value>,
    /// What the Forgejo dispatch endpoint answers (204 = started).
    pub dispatch_status: u16,
}

pub fn make_files() -> BTreeMap<String, Vec<u8>> {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    files.insert("tool-linux".into(), b"binary-one".repeat(1000));
    files.insert("tool-linux.tar.gz".into(), b"tarball".repeat(5000));
    files.insert("tool.receipt.json".into(), br#"{"ok":true}"#.to_vec());
    add_sums(&mut files);
    files
}

/// (Re)write SHA256SUMS to cover every other file.
pub fn add_sums(files: &mut BTreeMap<String, Vec<u8>>) {
    files.remove("SHA256SUMS");
    let sums: String = files
        .iter()
        .map(|(n, d)| format!("{}  {n}\n", sha(d)))
        .collect();
    files.insert("SHA256SUMS".into(), sums.into_bytes());
}

pub struct World {
    pub st: Arc<Mutex<State>>,
    pub forge: Fake,
    pub gh: Fake,
    pub gl: Fake,
    pub store: Fake,
    pub ctx: Ctx,
    pub repo: String,
    pub tag: String,
}

fn page(items: Vec<Value>, req: &Req) -> Resp {
    let per: usize = req
        .query
        .get("per_page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let number: usize = req
        .query
        .get("page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let slice: Vec<Value> = items
        .into_iter()
        .skip((number - 1) * per)
        .take(per)
        .collect();
    json_resp(200, Value::Array(slice))
}

impl World {
    pub async fn new() -> World {
        Self::with(|_| {}).await
    }

    pub async fn with(setup: impl FnOnce(&mut State)) -> World {
        let tag = "v0.1.0".to_owned();
        let files = make_files();
        let mut st = State {
            tag: tag.clone(),
            served: files.clone(),
            files,
            mirror: false,
            draft: false,
            g_tags: HashMap::from([(tag.clone(), COMMIT.to_owned())]),
            g_actions: false,
            g_releases: vec![],
            g_assets: HashMap::new(),
            g_limit: false,
            g_org_page: 200,
            g_next: 100,
            g_truncate: false,
            l_tags: HashMap::from([(tag.clone(), COMMIT.to_owned())]),
            l_builds: "disabled".into(),
            l_files: BTreeMap::new(),
            l_release: None,
            l_links: vec![],
            dispatch_status: 204,
        };
        setup(&mut st);
        let st = Arc::new(Mutex::new(st));
        let store = Fake::start({
            let st = st.clone();
            move |r| {
                let id: u64 = r.path.trim_start_matches("/blob/").parse().unwrap_or(0);
                let st = st.lock().unwrap();
                for assets in st.g_assets.values() {
                    for a in assets.values() {
                        if a.id == id {
                            return bytes_resp(200, a.data.clone());
                        }
                    }
                }
                bytes_resp(404, vec![])
            }
        })
        .await;
        let forge = Fake::start({
            let st = st.clone();
            move |r| forgejo(&st, r)
        })
        .await;
        let store_url = store.url.clone();
        let gh = Fake::start({
            let st = st.clone();
            move |r| github(&st, &store_url, r)
        })
        .await;
        let gl = Fake::start({
            let st = st.clone();
            move |r| gitlab(&st, r)
        })
        .await;
        let cfg: Config = serde_json::from_value(json!({
            "forge_api": format!("{}/api/v1", forge.url),
            "forge_hosts": ["forge.example.org"], "forge_web": forge.url,
            "github": {"api": gh.url, "uploads": gh.url, "web": format!("{}/web", gh.url), "orgs": ["example-fsl"], "min_write_interval_s": 0},
            "gitlab": {"api": format!("{}/api/v4", gl.url), "groups": ["example-fsl"]},
        }))
        .unwrap();
        let creds = Credentials::from_pairs(&[
            ("github_token", "ghp_fake"),
            ("gitlab_token", "glpat_fake"),
            ("hmac_forgejo", "s3cret"),
        ]);
        let ctx = Ctx::new(cfg, creds).unwrap();
        World {
            st,
            forge,
            gh,
            gl,
            store,
            ctx,
            repo: "example-fsl/ccvl".into(),
            tag,
        }
    }

    pub async fn sync(&self, dry: bool) -> cfrg_serve::release::Outcome {
        cfrg_serve::release::sync(&self.ctx, &self.repo, &self.tag, dry).await
    }

    pub fn gh_stored(&self) -> BTreeMap<String, Vec<u8>> {
        let st = self.st.lock().unwrap();
        st.g_assets
            .values()
            .flat_map(|a| a.iter().map(|(n, a)| (n.clone(), a.data.clone())))
            .collect()
    }
}

pub fn status_of(out: &cfrg_serve::release::Outcome, copy: &str) -> String {
    out.copies[copy].status.clone()
}

fn forgejo(st: &Arc<Mutex<State>>, r: &Req) -> Resp {
    let st = st.lock().unwrap();
    let p = r.path.as_str();
    if r.method == "POST"
        && p == "/api/v1/repos/example-infra/infra/actions/workflows/relay.yml/dispatches"
    {
        return bytes_resp(st.dispatch_status, vec![]);
    }
    if p == "/api/v1/repos/example-fsl/oldccvl" {
        // Forgejo answers a moved repository at its old path (redirect) with the current full name.
        return json_resp(
            200,
            json!({"mirror": st.mirror, "private": false, "full_name": "example-fsl/ccvl"}),
        );
    }
    if p == "/api/v1/repos/example-fsl/ccvl" {
        return json_resp(200, json!({"mirror": st.mirror, "private": false}));
    }
    if p == format!("/api/v1/repos/example-fsl/ccvl/releases/tags/{}", st.tag) {
        let assets: Vec<Value> = st
            .files
            .iter()
            .enumerate()
            .map(|(i, (n, d))| json!({"name": n, "size": d.len(), "id": i + 1}))
            .collect();
        return json_resp(
            200,
            json!({"tag_name": st.tag, "name": "ccvl 0.1.0", "body": "Notes\r\nline two", "draft": st.draft, "prerelease": false, "assets": assets}),
        );
    }
    if p == format!("/api/v1/repos/example-fsl/ccvl/tags/{}", st.tag) {
        return json_resp(200, json!({"commit": {"sha": COMMIT}}));
    }
    if let Some(name) = p.strip_prefix(&format!("/example-fsl/ccvl/releases/download/{}/", st.tag))
    {
        return match st.served.get(&unquote(name)) {
            Some(data) => bytes_resp(200, data.clone()),
            None => bytes_resp(404, vec![]),
        };
    }
    json_resp(404, json!({"message": "not found"}))
}

fn github(st: &Arc<Mutex<State>>, store_url: &str, r: &Req) -> Resp {
    let mut st = st.lock().unwrap();
    let parts: Vec<&str> = r.path.trim_start_matches('/').split('/').collect();
    let m = r.method.as_str();
    if m != "GET" && st.g_limit {
        return Resp {
            status: 403,
            headers: vec![("Retry-After".into(), "60".into())],
            body: br#"{"message":"You have exceeded a secondary rate limit"}"#.to_vec(),
        };
    }
    match (m, parts.as_slice()) {
        ("GET", ["web", "example-fsl"]) => bytes_resp(st.g_org_page, b"<html></html>".to_vec()),
        ("GET", ["repos", "example-fsl", "ccvl"]) => json_resp(200, json!({"archived": false})),
        ("GET", ["repos", "example-fsl", "ccvl", "git", "ref", "tags", tag]) => {
            match st.g_tags.get(&unquote(tag)) {
                Some(sha) => json_resp(200, json!({"object": {"type": "commit", "sha": sha}})),
                None => json_resp(404, json!({"message": "Not Found"})),
            }
        }
        ("GET", ["repos", "example-fsl", "ccvl", "actions", "permissions"]) => {
            json_resp(200, json!({"enabled": st.g_actions}))
        }
        ("GET", ["repos", "example-fsl", "ccvl", "releases"]) => {
            json_resp(200, Value::Array(st.g_releases.clone()))
        }
        ("POST", ["repos", "example-fsl", "ccvl", "releases"]) => {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            st.g_next += 1;
            let id = st.g_next;
            let rel = json!({"id": id, "tag_name": body["tag_name"], "name": body["name"], "body": body["body"], "draft": body["draft"], "prerelease": body["prerelease"]});
            st.g_releases.push(rel.clone());
            st.g_assets.insert(id, BTreeMap::new());
            json_resp(201, rel)
        }
        ("PATCH", ["repos", "example-fsl", "ccvl", "releases", id]) => {
            let id: u64 = id.parse().unwrap();
            let patch: Value = serde_json::from_slice(&r.body).unwrap();
            let rel = st.g_releases.iter_mut().find(|x| x["id"] == id).unwrap();
            for (k, v) in patch.as_object().unwrap() {
                rel[k] = v.clone();
            }
            json_resp(200, rel.clone())
        }
        ("GET", ["repos", "example-fsl", "ccvl", "releases", id, "assets"]) => {
            let id: u64 = id.parse().unwrap();
            let items: Vec<Value> = st
                .g_assets
                .get(&id)
                .map(|a| a.values().map(GAsset::json).collect())
                .unwrap_or_default();
            let number: usize = r
                .query
                .get("page")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            json_resp(200, Value::Array(if number == 1 { items } else { vec![] }))
        }
        ("POST", ["repos", "example-fsl", "ccvl", "releases", id, "assets"]) => {
            let id: u64 = id.parse().unwrap();
            let name = r.query["name"].clone();
            st.g_next += 1;
            let truncate = st.g_truncate;
            let next = st.g_next;
            let asset = GAsset {
                id: next,
                name: name.clone(),
                state: if truncate {
                    "starter".into()
                } else {
                    "uploaded".into()
                },
                size: if truncate { 0 } else { r.body.len() as u64 },
                digest: Some(format!("sha256:{}", sha(&r.body))),
                data: r.body.clone(),
            };
            let out = asset.json();
            st.g_assets.get_mut(&id).unwrap().insert(name, asset);
            json_resp(201, out)
        }
        ("DELETE", ["repos", "example-fsl", "ccvl", "releases", "assets", id]) => {
            let id: u64 = id.parse().unwrap();
            for assets in st.g_assets.values_mut() {
                assets.retain(|_, a| a.id != id);
            }
            bytes_resp(204, vec![])
        }
        ("GET", ["repos", "example-fsl", "ccvl", "releases", "assets", id]) => Resp {
            status: 302,
            headers: vec![("Location".into(), format!("{store_url}/blob/{id}"))],
            body: vec![],
        },
        _ => json_resp(404, json!({"message": "not found"})),
    }
}

fn gitlab(st: &Arc<Mutex<State>>, r: &Req) -> Resp {
    let mut st = st.lock().unwrap();
    let m = r.method.as_str();
    let p = r.path.as_str();
    let tag = st.tag.clone();
    if m == "GET" && p == "/api/v4/projects/example-fsl%2Fccvl"
        || p == "/api/v4/projects/example-fsl/ccvl"
    {
        return json_resp(
            200,
            json!({"id": 77, "path": "ccvl", "archived": false, "builds_access_level": st.l_builds, "jobs_enabled": st.l_builds != "disabled"}),
        );
    }
    let Some(rest) = p.strip_prefix("/api/v4/projects/77/") else {
        return json_resp(404, json!({"message": "not found"}));
    };
    let rest = rest.to_owned();
    let segs: Vec<&str> = rest.split('/').collect();
    match (m, segs.as_slice()) {
        ("GET", ["repository", "tags", t]) => match st.l_tags.get(&unquote(t)) {
            Some(id) => json_resp(200, json!({"name": unquote(t), "commit": {"id": id}})),
            None => json_resp(404, json!({"message": "404 Tag Not Found"})),
        },
        ("GET", ["packages"]) => page(
            if st.l_files.is_empty() {
                vec![]
            } else {
                vec![json!({"id": 5, "name": "ccvl", "version": tag})]
            },
            r,
        ),
        ("GET", ["packages", "5", "package_files"]) => {
            let items = st
                .l_files
                .iter()
                .flat_map(|(n, ds)| {
                    ds.iter().map(
                        move |d| json!({"file_name": n, "size": d.len(), "file_sha256": sha(d)}),
                    )
                })
                .collect();
            page(items, r)
        }
        ("PUT", ["packages", "generic", "ccvl", _, name]) => {
            st.l_files
                .entry(unquote(name))
                .or_default()
                .push(r.body.clone());
            json_resp(201, json!({"message": "201 Created"}))
        }
        ("GET", ["packages", "generic", "ccvl", _, name]) => match st.l_files.get(&unquote(name)) {
            Some(d) => bytes_resp(200, d.last().unwrap().clone()),
            None => bytes_resp(404, vec![]),
        },
        ("GET", ["releases", _]) => match &st.l_release {
            Some(rel) => json_resp(200, rel.clone()),
            None => json_resp(404, json!({"message": "404 Not found"})),
        },
        ("POST", ["releases"]) => {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            st.l_release = Some(
                json!({"tag_name": body["tag_name"], "name": body["name"], "description": body["description"]}),
            );
            st.l_links = body["assets"]["links"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            json_resp(201, st.l_release.clone().unwrap())
        }
        ("GET", ["releases", _, "assets", "links"]) => page(st.l_links.clone(), r),
        ("POST", ["releases", _, "assets", "links"]) => {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            st.l_links.push(body.clone());
            json_resp(201, body)
        }
        ("PUT", ["releases", _]) => {
            let patch: Value = serde_json::from_slice(&r.body).unwrap();
            let rel = st.l_release.as_mut().unwrap();
            for (k, v) in patch.as_object().unwrap() {
                rel[k] = v.clone();
            }
            json_resp(200, rel.clone())
        }
        _ => json_resp(404, json!({"message": "not found"})),
    }
}

/// A placement with one relayed repository: Forgejo 8, GitHub 4242 (example-fsl/ccvl), GitLab 55.
pub fn placement_text() -> String {
    let dest = |provider: &str, host: &str, id: &str, path: &str, ns: &str| {
        format!(
            r#"{{"provider":"{provider}","endpoint":{{"origin":"https://{host}","token_env":"T"}},"path":"{path}","namespace":"{ns}","repository_id":"{id}",
            "mirror_user":"u","password_env":"T","use_ssh":false,"interval_seconds":3600,"branch_filter":"","hold":null,"absent":false,"remote_name":null}}"#
        )
    };
    format!(
        r#"{{"native":{{"schema":1,"source":{{"origin":"https://forge.example.org","token_env":"T"}},"repositories":[
        {{"path":"example-fsl/ccvl","source_id":8,"private":false,"default_branch":"main","content":"profile","hold":null,"destinations":[{},{}]}}],
        "contributed":[]}}}}"#,
        dest(
            "github",
            "api.github.com",
            "4242",
            "example-fsl/ccvl",
            "example-fsl"
        ),
        dest("gitlab", "gitlab.com", "55", "example-fsl/ccvl", "2")
    )
}
