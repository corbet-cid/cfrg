//! The wiki leg (R40): when the Forgejo `wiki` system hook arrives, push that repository's wiki to its
//! GitLab copy, in-process, with the wiki deploy key. Fast-forward only; a diverged copy is held and
//! reported in the log, never forced. A contributed repository has no declared copies, so nothing happens.
use crate::{config::Config, config::Credentials};
use cfrg::native::{
    http::Http,
    wiki_sync::{Session, WikiOptions},
    Placement,
};
use serde_json::Value;
use std::{fs, path::Path, time::Duration};

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
    let secrets = |name: &str| -> cfrg::Result<String> {
        creds
            .get(name)
            .map(str::to_owned)
            .ok_or_else(|| format!("credential {name} is missing").into())
    };
    let mut io = Http::open(Path::new(&wiki.state), true).map_err(|e| e.to_string())?;
    let mut session = Session::new(
        &placement,
        WikiOptions {
            key: Some(wiki.key.clone().into()),
            known_hosts: Some(wiki.known_hosts.clone().into()),
            max_pushes: 1000,
            push_gap: Duration::ZERO,
            provider: Some("gitlab".into()),
            write: true,
        },
        &secrets,
    )
    .map_err(|e| e.to_string())?;
    // The hook names the repository by id; its current path comes from the forge listing (a rename moves it).
    let listed =
        cfgj::native::list_repositories(&mut io, &placement.source).map_err(|e| e.to_string())?;
    let Some(repo) = listed.iter().find(|r| r.id == id) else {
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
