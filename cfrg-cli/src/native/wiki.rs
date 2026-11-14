//! `cfrg native --operation wiki`: bring the wiki of every copy that can hold it in line with
//! the Forgejo wiki (R40; Forge-router §5.9). Without `--apply` it only reads and prints the
//! plan. The work is `cfrg::native::wiki_sync`, shared with `cfrg-serve`.
use super::Options;
use cfrg::{
    native::wiki_sync::{Session, WikiOptions},
    Result,
};
use std::{os::unix::fs::PermissionsExt, time::Duration};

pub fn run(options: &Options) -> Result<()> {
    let creds = super::creds::Creds::new(options.credentials_dir.clone());
    let placement = creds.placement(&std::fs::read(&options.placement)?)?;
    placement.validate()?;
    let mut io = creds.open(&options.state, options.apply)?;
    // ssh refuses a key file the group can read, and a mounted Secret is: a push uses a private copy (0600) of the key and the host pins.
    let private = tempfile::tempdir()?;
    let (mut key, mut known_hosts) = (options.wiki_key.clone(), options.wiki_known_hosts.clone());
    if options.apply {
        if let (Some(k), Some(h)) = (&options.wiki_key, &options.wiki_known_hosts) {
            let (pk, ph) = (
                private.path().join("key"),
                private.path().join("known_hosts"),
            );
            for (from, to) in [
                (k.clone(), pk.clone()),
                (k.with_extension("pub"), pk.with_extension("pub")),
                (h.clone(), ph.clone()),
            ] {
                std::fs::copy(&from, &to).map_err(|e| format!("{}: {e}", from.display()))?;
                std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o600))?;
            }
            (key, known_hosts) = (Some(pk), Some(ph));
        }
    }
    let secrets = |env: &str, path: &str| creds.token(env, path);
    let mut session = Session::new(
        &placement,
        WikiOptions {
            key,
            known_hosts,
            max_pushes: options.wiki_max_pushes,
            push_gap: Duration::from_secs(options.wiki_push_gap_secs),
            provider: options.provider.clone(),
            write: options.apply,
            key_id: options.wiki_key_id,
            github_kill_switch: options.github_kill_switch.clone(),
            github_paused_dir: options.github_paused_dir.clone(),
        },
        &secrets,
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
