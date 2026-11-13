//! The wiki leg (R40): when the Forgejo `wiki` system hook arrives, push that repository's wiki to its
//! GitLab copy, in-process, with the wiki deploy key. Fast-forward only; a diverged copy is held and
//! reported in the log, never forced. A contributed repository has no declared copies, so nothing happens.
use crate::{config::Config, config::Credentials};
use cfrg::native::{
    http::{expect, Auth, Http, Request, Transport},
    wiki_sync::{Session, WikiOptions},
    Endpoint, Placement,
};
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};

/// Synchronise the wiki of the repository with this Forgejo id; blocking (run it on a blocking thread).
pub fn sync(cfg: &Config, creds: &Credentials, forgejo_id: &str) -> Result<Value, String> {
    let wiki = cfg.wiki.as_ref().ok_or("no wiki leg configured")?;
    let id: u64 = forgejo_id
        .parse()
        .map_err(|_| "repository id is not a number")?;
    let document = fs::read(cfg.placement.as_deref().ok_or("no placement configured")?)
        .map_err(|e| format!("placement unreadable: {e}"))?;
    let placement = Placement::from_document(&document).map_err(|e| e.to_string())?;
    let Some(declared) = placement.repositories.iter().find(|r| r.source_id == id) else {
        return Ok(serde_json::json!({"state": "not-declared"}));
    };
    // The placement names its credentials by `token_env`; the credentials directory holds them under the bridge's names.
    let resolve = {
        let creds = creds.clone();
        move |name: &str| -> cfrg::Result<String> {
            let key = match name {
                "CFRG_FORGEJO_TOKEN" => "forge_token",
                "CFRG_GITLAB_TOKEN" => "gitlab_token",
                other => other,
            };
            creds
                .get(key)
                .map(str::to_owned)
                .ok_or_else(|| format!("credential {name} is missing").into())
        }
    };
    let secrets = resolve.clone();
    // ssh refuses a key file that is readable by the group, and a mounted Secret is: use a private copy.
    let private = tempfile::Builder::new()
        .prefix("cfrg-wiki-key-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let key = private.path().join("key");
    let known_hosts = private.path().join("known_hosts");
    for (from, to) in [
        (wiki.key.clone(), key.clone()),
        (format!("{}.pub", wiki.key), private.path().join("key.pub")),
        (wiki.known_hosts.clone(), known_hosts.clone()),
    ] {
        fs::copy(&from, &to).map_err(|e| format!("{from}: {e}"))?;
        fs::set_permissions(&to, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    let mut io = Http::open(Path::new(&wiki.state), true)
        .map_err(|e| e.to_string())?
        .with_secrets(Box::new(resolve));
    let mut session = Session::new(
        &placement,
        WikiOptions {
            key: Some(key),
            known_hosts: Some(known_hosts),
            max_pushes: 1000,
            push_gap: Duration::ZERO,
            provider: Some("gitlab".into()),
            write: true,
        },
        &secrets,
    )
    .map_err(|e| e.to_string())?;
    // The hook names the repository by id; its current path comes from one read (a rename moves it).
    let Some(repo) = repository(&mut io, &placement.source, id).map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({"state": "source-absent"}));
    };
    if !repo.has_wiki
        || !cfgj::native::wiki_exists(&mut io, &placement.source, &repo.path)
            .map_err(|e| e.to_string())?
    {
        return Ok(serde_json::json!({"state": "no-wiki"}));
    }
    session.repository(&mut io, &repo.path, declared);
    let report = session.report();
    if session.incomplete() {
        return Err(format!("wiki sync incomplete: {}", report["rows"]));
    }
    Ok(report)
}

/// One repository by its immutable id: `(current path, has_wiki)`, or `None` when it is gone. One read (R58).
fn repository(io: &mut dyn Transport, source: &Endpoint, id: u64) -> cfrg::Result<Option<Repo>> {
    let response = io.send(Request {
        endpoint: source.clone(),
        auth: Auth::Token,
        method: "GET",
        path: format!("/api/v1/repositories/{id}"),
        body: None,
        scope: format!("{}/api", source.origin),
        creation: false,
    })?;
    if response.status == 404 {
        return Ok(None);
    }
    let item = expect(response, &[200])?;
    let path = item["full_name"]
        .as_str()
        .ok_or("Missing repository name")?
        .to_ascii_lowercase();
    cfrg::native::path(&path, false)?;
    Ok(Some(Repo {
        path,
        has_wiki: item["has_wiki"] == true,
    }))
}

struct Repo {
    path: String,
    has_wiki: bool,
}
