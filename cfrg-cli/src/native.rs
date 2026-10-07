use cfrg::{
    native::{
        http::{Http, Transport},
        Content, Destination, Naming, Placement, Provider, Repository,
    },
    Result,
};
use clap::{Args, ValueEnum};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

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
    /// Only what a rename of the primary calls for: rename the destination,
    /// replace the mirror that points at its old name, record the new owner.
    Rename,
    /// Read-only audit of every declared name: the primary against the
    /// declared one, every destination (found by its recorded id) against the
    /// name it must carry. Holds and exceptions do not exempt a repository.
    Names,
}

pub fn run(options: Options) -> Result<()> {
    let (report, incomplete, result) = execute(&options)?;
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

/// Read-only verification of the declared push mirrors of one repository (or
/// all): source identity and the state of every mirror, nothing changed. Used
/// by `cfrg serve`.
pub fn verify(placement: &Path, state: &Path, repository: Option<&str>) -> Result<Value> {
    let options = Options {
        placement: placement.into(),
        state: state.into(),
        repository: repository.map(String::from),
        provider: None,
        existing_only: false,
        operation: Operation::Status,
        apply: false,
    };
    let (report, incomplete, result) = execute(&options)?;
    result?;
    Ok(json!({"complete": !incomplete, "repositories": report}))
}

/// Apply the renames the primary's names call for, and nothing else, to one
/// repository (or all). Used by `cfrg serve` when its policy allows writes.
pub fn rename(placement: &Path, state: &Path, repository: Option<&str>) -> Result<Value> {
    let options = Options {
        placement: placement.into(),
        state: state.into(),
        repository: repository.map(String::from),
        provider: None,
        existing_only: true,
        operation: Operation::Rename,
        apply: true,
    };
    let (report, incomplete, result) = execute(&options)?;
    result?;
    Ok(json!({"complete": !incomplete, "repositories": report}))
}

fn execute(options: &Options) -> Result<(Vec<Value>, bool, Result<()>)> {
    let placement = Placement::from_document(&fs::read(&options.placement)?)?;
    placement.validate()?;
    if options.apply
        && matches!(
            options.operation,
            Operation::Plan | Operation::Status | Operation::Names
        )
    {
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
        for declared in &placement.repositories {
            if options
                .repository
                .as_ref()
                .is_some_and(|path| *path != declared.path)
            {
                continue;
            }
            if placement.primary_of(&declared.path) != placement.source.origin.trim_end_matches('/')
            {
                // The placement moved this repository's primary elsewhere: Forgejo
                // is a receiver now, and `cfrg switch` owns its mirrors.
                report.push(json!({"source":declared.path, "state":"primary-elsewhere"}));
                continue;
            }
            if options.operation == Operation::Names {
                // One repository failing to answer never hides the others.
                match audit_names(&mut io, &placement, declared, &mut report) {
                    Ok(drift) => incomplete |= drift,
                    Err(error) => {
                        report.push(
                            json!({"source":declared.path, "state":"error", "error":error.to_string()}),
                        );
                        incomplete = true;
                    }
                }
                continue;
            }
            let source_held = declared.hold.is_some()
                || declared.content != Content::NativeGit
                || declared
                    .path
                    .rsplit('/')
                    .next()
                    .is_some_and(|p| p.starts_with('.'));
            if source_held {
                report.push(json!({"source":declared.path, "state":"exception", "reason":declared.hold.as_deref().unwrap_or("content/profile exception")}));
                incomplete = true;
                let prefix = format!("{}/{}/", placement.source.origin, declared.source_id);
                if !declared
                    .destinations
                    .iter()
                    .any(|d| d.remote_name.is_some())
                    && !io.state.owned.keys().any(|k| k.starts_with(&prefix))
                {
                    continue;
                }
            }
            // The primary is found by its immutable id: a repository renamed on
            // Forgejo since the placement was written is a finding, and its
            // destinations follow the name it has now.
            let current = cfgj::native::Mirrors {
                endpoint: &placement.source,
                repository: declared,
            }
            .verify_source(&mut io)?;
            let mut followed = declared.clone();
            if current != declared.path {
                report.push(
                    json!({"source":current, "declared":declared.path, "state":"source-renamed"}),
                );
                incomplete = true;
                followed.path = current;
            }
            let repo = &followed;
            let api = cfgj::native::Mirrors {
                endpoint: &placement.source,
                repository: repo,
            };
            let mut mirrors = api.list(&mut io)?;
            let mut managed = Vec::new();
            for declared_dest in &repo.destinations {
                if options.provider.as_deref().is_some_and(|selected| {
                    selected
                        != match declared_dest.provider {
                            Provider::Gitlab => "gitlab",
                            Provider::Bitbucket => "bitbucket",
                        }
                }) {
                    continue;
                }
                // Where this destination must live now, and every name it may
                // still be known under (the file's last record, the live name).
                let mut dest = declared_dest.clone();
                dest.path = repo.destination_path(declared_dest);
                let url = destination_url(&dest, &dest.path);
                let mut known = names(&dest.path, &declared_dest.recorded_path, None);
                let key =
                    ownership_key(&placement.source.origin, repo.source_id, &dest, &dest.path);
                let (mut mirror, owner) =
                    locate(&io, &placement.source.origin, repo, &dest, &mirrors, &known)?;
                // An owned mirror whose address is none of this destination's
                // names: the destination was renamed (an address cannot be edited).
                let mut stale_owned = stale_of(&mirrors, &mirror, &owner);
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
                    if stale_owned.is_some() {
                        return Err("Owned mirror points at another destination".into());
                    }
                    if let Some(value) = &mirror {
                        if options.apply && options.operation == Operation::Reconcile {
                            api.delete(
                                &mut io,
                                value["remote_name"].as_str().ok_or("Missing remote name")?,
                            )?;
                            mirrors.retain(|m| m["remote_name"] != value["remote_name"]);
                            forget_owned(&mut io, &placement.source.origin, repo, &dest, &known)?;
                        } else {
                            incomplete = true;
                        }
                    }
                    report.push(json!({"source":repo.path, "destination":dest.path, "state":if mirror.is_none() || (options.apply && options.operation == Operation::Reconcile) {"absent"} else {"delete-planned"}}));
                    continue;
                }
                if options.operation == Operation::Status {
                    // Status reads the primary only. A mirror that still points at
                    // an earlier name of the destination is a rename waiting for
                    // `cfrg native --operation rename --apply`.
                    let pending = stale_owned.is_some()
                        || mirror
                            .as_ref()
                            .is_some_and(|m| !cfgj::native::address_matches(m, &url));
                    let state = if pending { "rename-pending" } else { "checked" };
                    report.push(json!({"source":repo.path, "destination":dest.path, "state":state, "mirror":mirror.as_ref().or(stale_owned.as_ref()).map(cfgj::native::status)}));
                    incomplete |= pending || mirror.is_none();
                    continue;
                }
                let write = options.apply
                    && matches!(options.operation, Operation::Reconcile | Operation::Rename);
                let naming = match dest.provider {
                    Provider::Gitlab => cglb::native::naming(&mut io, &dest, write)?,
                    Provider::Bitbucket => cbkt::native::naming(&mut io, &dest, write)?,
                };
                let was = match naming {
                    Naming::Current => None,
                    Naming::Renamed { from } => {
                        report.push(json!({"source":repo.path, "destination":dest.path, "state":"renamed", "from":from}));
                        Some(from)
                    }
                    Naming::Planned { from } => {
                        report.push(json!({"source":repo.path, "destination":dest.path, "state":"rename-planned", "from":from}));
                        incomplete = true;
                        continue;
                    }
                    Naming::Blocked { from, reason } => {
                        report.push(json!({"source":repo.path, "destination":dest.path, "state":"rename-blocked", "from":from, "reason":reason}));
                        incomplete = true;
                        continue;
                    }
                };
                if let Some(from) = &was {
                    known = names(
                        &dest.path,
                        &declared_dest.recorded_path,
                        Some(from.as_str()),
                    );
                    let (found, owner) =
                        locate(&io, &placement.source.origin, repo, &dest, &mirrors, &known)?;
                    stale_owned = stale_of(&mirrors, &found, &owner);
                    mirror = found;
                }
                if mirror.is_none() {
                    if let Some(old) = stale_owned.take() {
                        // The destination's identity was just verified by id. The
                        // mirror is ours by record, it points at no name of this
                        // destination and at no other declared destination of
                        // this repository, on the same host: an earlier name.
                        let others: Vec<String> = repo
                            .destinations
                            .iter()
                            .filter(|other| !std::ptr::eq(*other, declared_dest))
                            .map(|other| destination_url(other, &repo.destination_path(other)))
                            .collect();
                        if dest.repository_id.is_none()
                            || !same_host(&old, &url)
                            || others
                                .iter()
                                .any(|other| cfgj::native::address_matches(&old, other))
                        {
                            return Err("Owned mirror points at another destination".into());
                        }
                        mirror = Some(old);
                    }
                }
                if options.operation == Operation::Rename {
                    let stale = was.is_some()
                        || mirror
                            .as_ref()
                            .is_some_and(|m| !cfgj::native::address_matches(m, &url));
                    if !stale {
                        report.push(
                            json!({"source":repo.path, "destination":dest.path, "state":"current"}),
                        );
                        continue;
                    }
                }
                // A rename never creates: a destination that is missing is reported.
                let allow_create = !options.existing_only && options.operation != Operation::Rename;
                let target = match dest.provider {
                    Provider::Gitlab => cglb::native::ensure_destination_repo(
                        &mut io,
                        repo,
                        &dest,
                        write,
                        allow_create,
                    )?,
                    Provider::Bitbucket => cbkt::native::ensure_destination_repo(
                        &mut io,
                        repo,
                        &dest,
                        write,
                        allow_create,
                    )?,
                };
                if dest.use_ssh && target.is_some() && mirror.is_none() && write {
                    let value = create_owned(&api, &mut io, &dest, &url, &key)?;
                    mirrors.push(value.clone());
                    mirror = Some(value);
                }
                let protected = if target.is_some() {
                    match dest.provider {
                        Provider::Gitlab if dest.use_ssh => match &mirror {
                            Some(value) => cglb::native::protect_ssh(&mut io, &dest, value, write)?,
                            None => false,
                        },
                        Provider::Gitlab => cglb::native::protect(&mut io, &dest, write)?,
                        Provider::Bitbucket => cbkt::native::protect(&mut io, &dest, write)?,
                    }
                } else {
                    false
                };
                if !protected {
                    report.push(json!({"source":repo.path, "destination":dest.path, "state":"destination-or-protection-required"}));
                    incomplete = true;
                    continue;
                }
                let mut replaced = None;
                if mirror
                    .as_ref()
                    .is_some_and(|m| !cfgj::native::matches(m, &dest, &url))
                {
                    if write {
                        // Forgejo has no update API: replace only an owned mirror,
                        // after destination protection is reverified.
                        let old = mirror.take().ok_or("Missing mirror")?;
                        let name = old["remote_name"].as_str().ok_or("Missing remote name")?;
                        api.delete(&mut io, name)?;
                        mirrors.retain(|m| m["remote_name"] != old["remote_name"]);
                        forget_owned(&mut io, &placement.source.origin, repo, &dest, &known)?;
                        replaced = Some(name.to_string());
                    } else {
                        report.push(json!({"source":repo.path, "destination":dest.path, "state":"replace-planned"}));
                        incomplete = true;
                        continue;
                    }
                }
                if mirror.is_none() && write {
                    let value = create_owned(&api, &mut io, &dest, &url, &key)?;
                    if dest.use_ssh {
                        cglb::native::protect_ssh(&mut io, &dest, &value, true)?;
                        if let Some(old) = &replaced {
                            // The new key is enrolled: the old mirror's key is an orphan.
                            cglb::native::forget_key(&mut io, &dest, old)?;
                        }
                    }
                    mirrors.push(value.clone());
                    mirror = Some(value);
                }
                if let Some(value) = &mirror {
                    let target = target.as_ref().ok_or("Missing verified destination")?;
                    let default_branch_ready = match dest.provider {
                        Provider::Gitlab => cglb::native::finalize_default_branch(
                            &mut io, repo, &dest, target, write,
                        )?,
                        Provider::Bitbucket => cbkt::native::finalize_default_branch(
                            &mut io, repo, &dest, target, write,
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
    Ok((report, incomplete, result))
}

/// `--operation names`: compare the names of one repository and its destinations.
/// True when anything differs.
fn audit_names(
    io: &mut dyn Transport,
    placement: &Placement,
    declared: &Repository,
    report: &mut Vec<Value>,
) -> Result<bool> {
    let mut drift = false;
    let current = cfgj::native::Mirrors {
        endpoint: &placement.source,
        repository: declared,
    }
    .verify_source(io)?;
    let mut repo = declared.clone();
    if current != declared.path {
        report.push(json!({"source":current, "declared":declared.path, "state":"source-renamed"}));
        repo.path = current;
        drift = true;
    }
    for declared_dest in &repo.destinations {
        let mut dest = declared_dest.clone();
        dest.path = repo.destination_path(declared_dest);
        let row = |state: &str, extra: Value| {
            let mut row = json!({"source":repo.path, "destination":dest.path, "provider":dest.provider, "pinned":dest.pinned(), "state":state});
            if let (Some(fields), Some(more)) = (row.as_object_mut(), extra.as_object()) {
                fields.extend(more.clone());
            }
            row
        };
        if dest.repository_id.is_none() {
            report.push(row("no-recorded-id", json!({})));
            continue;
        }
        let naming = match dest.provider {
            Provider::Gitlab => cglb::native::naming(io, &dest, false)?,
            Provider::Bitbucket => cbkt::native::naming(io, &dest, false)?,
        };
        drift |= !matches!(naming, Naming::Current);
        report.push(match naming {
            Naming::Current => row("current", json!({})),
            Naming::Planned { from } => row("rename-planned", json!({"from":from})),
            Naming::Renamed { from } => row("renamed", json!({"from":from})),
            Naming::Blocked { from, reason } => {
                row("rename-blocked", json!({"from":from, "reason":reason}))
            }
        });
    }
    Ok(drift)
}

/// The Forgejo mirror of a destination under any of its `known` names, and the
/// owner recorded for it. A mirror found by address must be the recorded owner.
fn locate(
    io: &Http,
    origin: &str,
    repo: &Repository,
    dest: &Destination,
    mirrors: &[Value],
    known: &[String],
) -> Result<(Option<Value>, Option<String>)> {
    let mirror = mirror_named(mirrors, dest, known)?;
    // Recorded under any of its names; failing that under its identity (the
    // repository and the provider, whatever the path was then); the declared
    // `remote_name` last, because it goes stale when a mirror is replaced.
    let owner = owner_of(
        &io.state.owned,
        &keys_of(origin, repo.source_id, dest, known),
    )
    .or_else(|| identity_owner(&io.state.owned, origin, repo, dest))
    .or_else(|| dest.remote_name.clone());
    if let Some(value) = &mirror {
        if owner.as_deref() != value["remote_name"].as_str() {
            return Err("Existing mirror has no matching declared/recorded ownership; declare remote_name after review".into());
        }
    }
    Ok((mirror, owner))
}

/// The recorded owner's mirror when no mirror carries any known name.
fn stale_of(mirrors: &[Value], mirror: &Option<Value>, owner: &Option<String>) -> Option<Value> {
    if mirror.is_some() {
        return None;
    }
    let name = owner.as_ref()?;
    mirrors.iter().find(|m| m["remote_name"] == *name).cloned()
}

/// Names a destination may be known under: the wanted one first, then the
/// last one the file recorded and the live one (a rename the Forgejo mirror has
/// not followed yet).
fn names(wanted: &str, recorded: &str, live: Option<&str>) -> Vec<String> {
    let mut all = vec![wanted.to_string()];
    for name in [recorded].into_iter().chain(live) {
        if !name.is_empty() && !all.iter().any(|known| known == name) {
            all.push(name.to_string());
        }
    }
    all
}

/// The clone URL of `dest` under `path`.
fn destination_url(dest: &Destination, path: &str) -> String {
    let mut named = dest.clone();
    named.path = path.to_string();
    match dest.provider {
        Provider::Gitlab => cglb::native::clone_url(&named),
        Provider::Bitbucket => cbkt::native::clone_url(&named),
    }
}

/// The key under which the state records which Forgejo mirror is ours.
fn ownership_key(origin: &str, source_id: u64, dest: &Destination, path: &str) -> String {
    format!("{origin}/{source_id}/{}/{path}", dest.endpoint.origin)
}

fn keys_of(origin: &str, source_id: u64, dest: &Destination, names: &[String]) -> Vec<String> {
    names
        .iter()
        .map(|name| ownership_key(origin, source_id, dest, name))
        .collect()
}

/// The owner recorded for a destination under any of its names.
fn owner_of(owned: &BTreeMap<String, String>, keys: &[String]) -> Option<String> {
    keys.iter().find_map(|key| owned.get(key)).cloned()
}

/// The owner recorded for the only destination this repository has on that
/// provider, whatever path the record was made under.
fn identity_owner(
    owned: &BTreeMap<String, String>,
    origin: &str,
    repo: &Repository,
    dest: &Destination,
) -> Option<String> {
    let siblings = repo
        .destinations
        .iter()
        .filter(|other| other.endpoint.origin == dest.endpoint.origin)
        .count();
    if siblings != 1 {
        return None;
    }
    let prefix = format!("{origin}/{}/{}/", repo.source_id, dest.endpoint.origin);
    let mut found = owned.iter().filter(|(key, _)| key.starts_with(&prefix));
    match (found.next(), found.next()) {
        (Some((_, name)), None) => Some(name.clone()),
        _ => None,
    }
}

/// Drop the records of a destination (a replaced or deleted mirror): those under
/// any of its names and, when it is the repository's only destination on that
/// provider, every record made for the provider whatever the path was then.
fn forget_owned(
    io: &mut Http,
    origin: &str,
    repo: &Repository,
    dest: &Destination,
    names: &[String],
) -> Result<()> {
    for key in keys_of(origin, repo.source_id, dest, names) {
        io.state.owned.remove(&key);
    }
    let sole = repo
        .destinations
        .iter()
        .filter(|other| other.endpoint.origin == dest.endpoint.origin)
        .count()
        == 1;
    if sole {
        let prefix = format!("{origin}/{}/{}/", repo.source_id, dest.endpoint.origin);
        io.state.owned.retain(|key, _| !key.starts_with(&prefix));
    }
    io.save()
}

/// The Forgejo push mirror that points at the destination under any of `names`.
fn mirror_named(mirrors: &[Value], dest: &Destination, names: &[String]) -> Result<Option<Value>> {
    let urls: Vec<String> = names
        .iter()
        .map(|name| destination_url(dest, name))
        .collect();
    let mut found: Vec<Value> = mirrors
        .iter()
        .filter(|m| urls.iter().any(|url| cfgj::native::address_matches(m, url)))
        .cloned()
        .collect();
    if found.len() > 1 {
        return Err("Multiple mirrors for one destination; explicit repair required".into());
    }
    Ok(found.pop())
}

/// Whether the mirror's address and `url` name the same host (userinfo ignored).
fn same_host(mirror: &Value, url: &str) -> bool {
    fn host(address: &str) -> Option<&str> {
        let rest = address.split_once("://")?.1;
        let authority = rest.split('/').next()?;
        authority.rsplit('@').next()
    }
    mirror["remote_address"]
        .as_str()
        .and_then(host)
        .is_some_and(|found| Some(found) == host(url))
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
    fn gitlab(path: &str) -> Destination {
        serde_json::from_value(json!({"provider":"gitlab","endpoint":{"origin":"https://gitlab.example","token_env":"TOKEN"},"path":path,"namespace":"2","repository_id":"7","mirror_user":"9","password_env":"TOKEN","use_ssh":true,"interval_seconds":3600})).unwrap()
    }
    fn mirror(name: &str, address: &str) -> Value {
        json!({"remote_name":name,"remote_address":address})
    }

    #[test]
    fn a_destination_is_known_under_its_wanted_recorded_and_live_names() {
        assert_eq!(
            names("t/new", "t/old", Some("t/live")),
            ["t/new", "t/old", "t/live"]
        );
        assert_eq!(names("t/new", "t/new", Some("t/new")), ["t/new"]);
        assert_eq!(names("t/new", "", None), ["t/new"]);
    }

    #[test]
    fn the_mirror_of_a_renamed_destination_is_found_under_its_old_name() {
        let dest = gitlab("team/new");
        let old = mirror("remote_mirror_a", "ssh://gitlab.example/team/old.git");
        let other = mirror("remote_mirror_b", "https://bitbucket.org/team/new.git");
        let mirrors = [old.clone(), other];
        let wanted_only = names("team/new", "", None);
        assert!(mirror_named(&mirrors, &dest, &wanted_only)
            .unwrap()
            .is_none());
        let both = names("team/new", "team/old", None);
        assert_eq!(mirror_named(&mirrors, &dest, &both).unwrap(), Some(old));
        // Two mirrors for one destination are never guessed between.
        let twin = mirror("remote_mirror_c", "ssh://gitlab.example/team/new.git");
        let twins = [
            mirror("remote_mirror_a", "ssh://gitlab.example/team/old.git"),
            twin,
        ];
        assert!(mirror_named(&twins, &dest, &both).is_err());
    }

    #[test]
    fn ownership_is_found_under_any_name_or_the_identity_of_the_destination() {
        let dest = gitlab("team/new");
        let known = names("team/new", "team/old", None);
        let keys = keys_of("https://forge.example", 5, &dest, &known);
        assert_eq!(
            keys[1],
            "https://forge.example/5/https://gitlab.example/team/old"
        );
        let mut owned = BTreeMap::new();
        owned.insert(keys[1].clone(), "remote_mirror_a".to_string());
        assert_eq!(owner_of(&owned, &keys).as_deref(), Some("remote_mirror_a"));
        assert_eq!(owner_of(&BTreeMap::new(), &keys), None);
        // A record made under a name the destination never had here is still its record.
        let mut elsewhere = BTreeMap::new();
        elsewhere.insert(
            "https://forge.example/5/https://gitlab.example/team/older".to_string(),
            "remote_mirror_b".to_string(),
        );
        let mut repo: Repository = serde_json::from_value(json!({"path":"team/new","source_id":5,"private":true,"default_branch":"main","content":"native-git","destinations":[]})).unwrap();
        repo.destinations.push(dest.clone());
        assert_eq!(
            identity_owner(&elsewhere, "https://forge.example", &repo, &dest).as_deref(),
            Some("remote_mirror_b")
        );
        // Two records, another repository, or two destinations on the provider: no guess.
        elsewhere.insert(
            "https://forge.example/5/https://gitlab.example/team/oldest".to_string(),
            "remote_mirror_c".to_string(),
        );
        assert_eq!(
            identity_owner(&elsewhere, "https://forge.example", &repo, &dest),
            None
        );
        let other = BTreeMap::from([(
            "https://forge.example/6/https://gitlab.example/team/new".to_string(),
            "remote_mirror_d".to_string(),
        )]);
        assert_eq!(
            identity_owner(&other, "https://forge.example", &repo, &dest),
            None
        );
        repo.destinations.push(gitlab("team/second"));
        let single = BTreeMap::from([(
            "https://forge.example/5/https://gitlab.example/team/new".to_string(),
            "remote_mirror_e".to_string(),
        )]);
        assert_eq!(
            identity_owner(&single, "https://forge.example", &repo, &dest),
            None
        );
    }

    #[test]
    fn an_owned_mirror_with_no_known_name_is_stale_only_without_a_found_mirror() {
        let old = mirror("remote_mirror_a", "ssh://gitlab.example/team/gone.git");
        let mirrors = [old.clone()];
        let owner = Some("remote_mirror_a".to_string());
        assert_eq!(stale_of(&mirrors, &None, &owner), Some(old.clone()));
        assert_eq!(stale_of(&mirrors, &Some(old), &owner), None);
        assert_eq!(stale_of(&mirrors, &None, &None), None);
        assert_eq!(
            stale_of(&mirrors, &None, &Some("remote_mirror_x".to_string())),
            None
        );
    }

    #[test]
    fn a_stale_mirror_must_sit_on_the_destinations_host() {
        let url = "ssh://git@gitlab.example/team/new.git";
        let on_host = mirror("m", "ssh://gitlab.example/team/old.git");
        assert!(same_host(&on_host, url));
        let with_userinfo = mirror("m", "https://user:pw@bitbucket.org/team/old.git");
        assert!(same_host(
            &with_userinfo,
            "https://bitbucket.org/team/new.git"
        ));
        assert!(!same_host(&on_host, "https://bitbucket.org/team/new.git"));
        assert!(!same_host(&json!({"remote_name":"m"}), url));
    }
    /// Answers by method and path; an unexpected request fails the test.
    struct Routes(Vec<(&'static str, &'static str, u16, Value)>);
    impl Transport for Routes {
        fn send(
            &mut self,
            request: cfrg::native::http::Request,
        ) -> Result<cfrg::native::http::Response> {
            let route = self
                .0
                .iter()
                .find(|(method, path, _, _)| *method == request.method && *path == request.path)
                .unwrap_or_else(|| panic!("unexpected {} {}", request.method, request.path));
            Ok(cfrg::native::http::Response {
                status: route.2,
                body: route.3.clone(),
            })
        }
    }

    #[test]
    fn the_names_audit_finds_every_destination_that_lags_its_primary() {
        let dest = |provider: &str, origin: &str, extra: &str| {
            format!(
                r#"{{"provider":"{provider}","endpoint":{{"origin":"{origin}","token_env":"T"}},"namespace":"2","mirror_user":"9","password_env":"T","interval_seconds":3600,"hold":null,"remote_name":null,{extra}}}"#
            )
        };
        let document = format!(
            r#"{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"repositories":[
            {{"path":"team/old","source_id":1,"private":true,"default_branch":"main","content":"lfs","hold":"held does not exempt","destinations":[{},{},{},{}]}}]}}"#,
            dest(
                "gitlab",
                "https://gitlab.example",
                r#""path":"team/old","repository_id":"7""#
            ),
            dest(
                "bitbucket",
                "https://api.bitbucket.org",
                r#""path":"team/old","repository_id":"{u-1}""#
            ),
            dest(
                "gitlab",
                "https://gitlab.example",
                r#""path":"keep/it","path_reason":"the archive keeps it","repository_id":"8""#
            ),
            dest("gitlab", "https://gitlab.other", r#""repository_id":null"#),
        );
        let placement = Placement::from_document(document.as_bytes()).unwrap();
        placement.validate().unwrap();
        let mut io = Routes(vec![
            (
                "GET",
                "/api/v1/repositories/1",
                200,
                json!({"id":1,"full_name":"team/new","private":true,"default_branch":"main","mirror":false}),
            ),
            (
                "GET",
                "/api/v4/projects/7",
                200,
                json!({"id":7,"path_with_namespace":"team/old","name":"old","path":"old"}),
            ),
            ("GET", "/api/v4/projects/team%2Fnew", 404, Value::Null),
            (
                "GET",
                "/2.0/repositories/team/%7Bu-1%7D",
                200,
                json!({"uuid":"{u-1}","full_name":"team/old"}),
            ),
            ("GET", "/2.0/repositories/team/new", 404, Value::Null),
            (
                "GET",
                "/api/v4/projects/8",
                200,
                json!({"id":8,"path_with_namespace":"keep/it","name":"it","path":"it"}),
            ),
        ]);
        let mut report = Vec::new();
        let drift =
            audit_names(&mut io, &placement, &placement.repositories[0], &mut report).unwrap();
        assert!(drift);
        let states: Vec<_> = report
            .iter()
            .map(|row| (row["state"].as_str().unwrap(), row["destination"].as_str()))
            .collect();
        assert_eq!(
            states,
            [
                ("source-renamed", None),
                ("rename-planned", Some("team/new")),
                ("rename-planned", Some("team/new")),
                ("current", Some("keep/it")),
                ("no-recorded-id", Some("team/new")),
            ]
        );
        assert_eq!(report[1]["from"], "team/old");
        assert_eq!(report[3]["pinned"], true);
    }
}
