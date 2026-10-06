use cfrg::{
    native::{http::Http, Content, Placement, Provider},
    Result,
};
use clap::{Args, ValueEnum};
use serde_json::{json, Value};
use std::{fs, path::PathBuf};

#[derive(Args)]
pub struct Options {
    #[arg(long)]
    placement: PathBuf,
    /// Persistent windows, uncertain intents and owned native remote identities.
    #[arg(long)]
    state: PathBuf,
    #[arg(long)]
    repository: Option<String>,
    /// Reconcile one provider without changing other declared destinations.
    #[arg(long, value_parser = ["gitlab", "bitbucket"])]
    provider: Option<String>,
    /// Existing projects still reconcile; missing repositories are only reported.
    #[arg(long)]
    existing_only: bool,
    #[arg(long, value_enum, default_value = "plan")]
    operation: Operation,
    #[arg(long)]
    apply: bool,
}
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Operation {
    Plan,
    Reconcile,
    Status,
    SyncNow,
}

pub fn run(options: Options) -> Result<()> {
    let placement: Placement = serde_json::from_slice(&fs::read(&options.placement)?)?;
    placement.validate()?;
    if options.apply && matches!(options.operation, Operation::Plan | Operation::Status) {
        return Err("Plan and status cannot apply changes".into());
    }
    if options
        .repository
        .as_ref()
        .is_some_and(|path| !placement.repositories.iter().any(|r| r.path == *path))
    {
        return Err("Selected repository is not declared".into());
    }
    let mut io = Http::open(&options.state, options.apply)?;
    let mut report = Vec::new();
    let mut incomplete = false;
    let result = (|| -> Result<()> {
        for repo in &placement.repositories {
            if options
                .repository
                .as_ref()
                .is_some_and(|path| *path != repo.path)
            {
                continue;
            }
            let source_held = repo.hold.is_some()
                || repo.content != Content::NativeGit
                || repo
                    .path
                    .rsplit('/')
                    .next()
                    .is_some_and(|p| p.starts_with('.'));
            if source_held {
                report.push(json!({"source":repo.path, "state":"exception", "reason":repo.hold.as_deref().unwrap_or("content/profile exception")}));
                incomplete = true;
                let prefix = format!("{}/{}/", placement.source.origin, repo.source_id);
                if !repo.destinations.iter().any(|d| d.remote_name.is_some())
                    && !io.state.owned.keys().any(|k| k.starts_with(&prefix))
                {
                    continue;
                }
            }
            let api = cfgj::native::Mirrors {
                endpoint: &placement.source,
                repository: repo,
            };
            api.verify_source(&mut io)?;
            let mut mirrors = api.list(&mut io)?;
            let mut managed = Vec::new();
            for dest in &repo.destinations {
                if options.provider.as_deref().is_some_and(|selected| {
                    selected
                        != match dest.provider {
                            Provider::Gitlab => "gitlab",
                            Provider::Bitbucket => "bitbucket",
                        }
                }) {
                    continue;
                }
                let url = match dest.provider {
                    Provider::Gitlab => cglb::native::clone_url(dest),
                    Provider::Bitbucket => cbkt::native::clone_url(dest),
                };
                let key = format!(
                    "{}/{}/{}/{}",
                    placement.source.origin, repo.source_id, dest.endpoint.origin, dest.path
                );
                let owner = io
                    .state
                    .owned
                    .get(&key)
                    .or(dest.remote_name.as_ref())
                    .cloned();
                let found: Vec<_> = mirrors
                    .iter()
                    .filter(|m| cfgj::native::address_matches(m, &url))
                    .cloned()
                    .collect();
                if found.len() > 1 {
                    return Err(
                        "Multiple mirrors for one destination; explicit repair required".into(),
                    );
                }
                let mut mirror = found.first().cloned();
                if let Some(value) = &mirror {
                    if owner.as_deref() != value["remote_name"].as_str() {
                        return Err("Existing mirror has no matching declared/recorded ownership; declare remote_name after review".into());
                    }
                } else if let Some(name) = &owner {
                    if mirrors.iter().any(|m| m["remote_name"] == *name) {
                        return Err("Owned mirror points at another destination".into());
                    }
                }
                let workspace_scope = format!(
                    "{}/workspace/{}",
                    dest.endpoint.origin,
                    dest.path.split('/').next().ok_or("Missing workspace")?
                );
                if dest.provider == Provider::Bitbucket
                    && mirror.as_ref().is_some_and(|m| {
                        m["last_error"]
                            .as_str()
                            .is_some_and(|s| s.contains("HTTP 402") || s.contains("error: 402"))
                    })
                {
                    io.state.windows.insert(
                        workspace_scope.clone(),
                        cfrg::native::http::Window {
                            observed_at: cfrg::native::http::now()?,
                            status: 402,
                            retry_at: None,
                        },
                    );
                    io.save()?;
                }
                let held = source_held
                    || dest.hold.is_some()
                    || io
                        .state
                        .windows
                        .get(&workspace_scope)
                        .is_some_and(|w| w.status == 402);
                if held {
                    incomplete = true;
                }
                if dest.absent || held {
                    if let Some(value) = &mirror {
                        if options.apply && options.operation == Operation::Reconcile {
                            api.delete(
                                &mut io,
                                value["remote_name"].as_str().ok_or("Missing remote name")?,
                            )?;
                            mirrors.retain(|m| m["remote_name"] != value["remote_name"]);
                            io.state.owned.remove(&key);
                            io.save()?;
                        } else {
                            incomplete = true;
                        }
                    }
                    report.push(json!({"source":repo.path, "destination":dest.path, "state":if mirror.is_none() || (options.apply && options.operation == Operation::Reconcile) {"absent"} else {"delete-planned"}}));
                    continue;
                }
                if options.operation == Operation::Status {
                    report.push(json!({"source":repo.path, "destination":dest.path, "mirror":mirror.as_ref().map(cfgj::native::status)}));
                    incomplete |= mirror.is_none();
                    continue;
                }
                let write = options.apply && options.operation == Operation::Reconcile;
                let target = match dest.provider {
                    Provider::Gitlab => cglb::native::ensure_destination_repo(
                        &mut io,
                        repo,
                        dest,
                        write,
                        !options.existing_only,
                    )?,
                    Provider::Bitbucket => cbkt::native::ensure_destination_repo(
                        &mut io,
                        repo,
                        dest,
                        write,
                        !options.existing_only,
                    )?,
                };
                if dest.use_ssh && target.is_some() && mirror.is_none() && write {
                    let value = create_owned(&api, &mut io, dest, &url, &key)?;
                    mirrors.push(value.clone());
                    mirror = Some(value);
                }
                let protected = if target.is_some() {
                    match dest.provider {
                        Provider::Gitlab if dest.use_ssh => match &mirror {
                            Some(value) => cglb::native::protect_ssh(&mut io, dest, value, write)?,
                            None => false,
                        },
                        Provider::Gitlab => cglb::native::protect(&mut io, dest, write)?,
                        Provider::Bitbucket => cbkt::native::protect(&mut io, dest, write)?,
                    }
                } else {
                    false
                };
                if !protected {
                    report.push(json!({"source":repo.path, "destination":dest.path, "state":"destination-or-protection-required"}));
                    incomplete = true;
                    continue;
                }
                if mirror
                    .as_ref()
                    .is_some_and(|m| !cfgj::native::matches(m, dest, &url))
                {
                    if write {
                        // Forgejo has no update API: replace only an owned mirror,
                        // after destination protection is reverified.
                        let old = mirror.take().ok_or("Missing mirror")?;
                        api.delete(
                            &mut io,
                            old["remote_name"].as_str().ok_or("Missing remote name")?,
                        )?;
                        mirrors.retain(|m| m["remote_name"] != old["remote_name"]);
                        io.state.owned.remove(&key);
                        io.save()?;
                    } else {
                        report.push(json!({"source":repo.path, "destination":dest.path, "state":"replace-planned"}));
                        incomplete = true;
                        continue;
                    }
                }
                if mirror.is_none() && write {
                    let value = create_owned(&api, &mut io, dest, &url, &key)?;
                    if dest.use_ssh {
                        cglb::native::protect_ssh(&mut io, dest, &value, true)?;
                    }
                    mirrors.push(value.clone());
                    mirror = Some(value);
                }
                if let Some(value) = &mirror {
                    let target = target.as_ref().ok_or("Missing verified destination")?;
                    let default_branch_ready = match dest.provider {
                        Provider::Gitlab => cglb::native::finalize_default_branch(
                            &mut io, repo, dest, target, write,
                        )?,
                        Provider::Bitbucket => cbkt::native::finalize_default_branch(
                            &mut io, repo, dest, target, write,
                        )?,
                    };
                    incomplete |= !default_branch_ready;
                    managed.push(value["remote_name"].clone());
                    report.push(json!({"source":repo.path, "destination":dest.path, "state":"configured", "default_branch_ready":default_branch_ready, "mirror":cfgj::native::status(value)}));
                } else {
                    incomplete = true;
                    report.push(
                    json!({"source":repo.path, "destination":dest.path, "state":"create-planned"}),
                );
                }
            }
            if options.operation == Operation::SyncNow {
                // This Forgejo endpoint synchronizes every remote; never trigger
                // undeclared, held, absent or unprotected destinations as a side effect.
                if !all_mirrors_managed(&mirrors, &managed) {
                    return Err("Sync-now would include an unmanaged or unready mirror".into());
                }
                if options.apply && !mirrors.is_empty() {
                    api.sync_now(&mut io)?;
                }
            }
        }
        Ok(())
    })();
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"complete":!incomplete && result.is_ok(), "repositories":report})
        )?
    );
    result?;
    if incomplete && options.apply {
        return Err("Native placement contains incomplete destinations or exceptions".into());
    }
    Ok(())
}

fn all_mirrors_managed(mirrors: &[Value], managed: &[Value]) -> bool {
    mirrors.iter().all(|m| managed.contains(&m["remote_name"]))
}

fn create_owned(
    api: &cfgj::native::Mirrors<'_>,
    io: &mut Http,
    dest: &cfrg::native::Destination,
    url: &str,
    key: &str,
) -> Result<Value> {
    let password = if dest.use_ssh {
        cglb::native::deny_branch_writes(io, dest)?;
        String::new()
    } else {
        let value = std::env::var(&dest.password_env)
            .map_err(|_| "Missing mirror credential environment reference")?;
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("Invalid mirror credential format".into());
        }
        value
    };
    let value = api.create(io, dest, url, &password)?;
    if !cfgj::native::matches(&value, dest, url) {
        return Err("Created mirror does not match placement; inspect before retry".into());
    }
    let name = value["remote_name"]
        .as_str()
        .ok_or("Missing created mirror remote name")?;
    cfrg::native::component(name)?;
    io.state.owned.insert(key.into(), name.into());
    io.save()?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sync_requires_every_actual_remote_but_not_absent_destinations() {
        let managed = vec![json!("remote_mirror_ready")];
        let ready = json!({"remote_name":"remote_mirror_ready"});
        assert!(all_mirrors_managed(std::slice::from_ref(&ready), &managed));
        assert!(!all_mirrors_managed(
            &[ready, json!({"remote_name":"remote_mirror_held"})],
            &managed
        ));
    }
}
