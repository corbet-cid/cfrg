//! `cfrg native --operation wiki`: bring the wiki of every copy that can hold it in line with
//! the Forgejo wiki (R40; Forge-router §5.9). Without `--apply` it only reads and prints the
//! plan. The work is `cfrg::native::wiki_sync`, shared with `cfrg-serve`.
use super::Options;
use cfrg::{
    native::{
        http::Http,
        wiki_sync::{Session, WikiOptions},
        Placement,
    },
    Result,
};
use std::{fs, time::Duration};

fn secret(env: &str) -> Result<String> {
    cfrg::native::env_name(env)?;
    std::env::var(env)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{env} is not set").into())
}

pub fn run(options: &Options) -> Result<()> {
    let placement = Placement::from_document(&fs::read(&options.placement)?)?;
    placement.validate()?;
    let mut io = Http::open(&options.state, options.apply)?;
    let mut session = Session::new(
        &placement,
        WikiOptions {
            key: options.wiki_key.clone(),
            known_hosts: options.wiki_known_hosts.clone(),
            max_pushes: options.wiki_max_pushes,
            push_gap: Duration::from_secs(options.wiki_push_gap_secs),
            provider: options.provider.clone(),
            write: options.apply,
            key_id: options.wiki_key_id,
        },
        &secret,
    )?;
    let listed = cfgj::native::list_repositories(&mut io, &placement.source)?;
    for repo in &listed {
        if options
            .repository
            .as_ref()
            .is_some_and(|only| !only.eq_ignore_ascii_case(&repo.path))
            || !repo.has_wiki
        {
            continue;
        }
        let Some(declared) = placement
            .repositories
            .iter()
            .find(|r| r.source_id == repo.id)
        else {
            continue;
        };
        if !cfgj::native::wiki_exists(&mut io, &placement.source, &repo.path)? {
            continue;
        }
        session.repository(&mut io, &repo.path, declared);
    }
    println!("{}", serde_json::to_string_pretty(&session.report())?);
    if session.incomplete() && options.apply {
        return Err("Some copy wikis were not brought in line; see the rows".into());
    }
    Ok(())
}
