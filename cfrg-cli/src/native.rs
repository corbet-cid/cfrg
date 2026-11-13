mod copy;
mod members;
mod protection;
mod settings;
mod wiki;

use cfrg::{
    native::{
        http::{Http, Pacing, Transport},
        owned_by, Content, Destination, Naming, Placement, Provider, Repository,
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
    /// Persistent windows and uncertain intents. Ownership of a mirror is never stored: it is
    /// derived from Forgejo's own mirror list and the placement (design 3.7).
    #[arg(long)]
    state: PathBuf,
    #[arg(long)]
    repository: Option<String>,
    /// Reconcile one provider without changing other declared destinations.
    #[arg(long, value_parser = ["gitlab", "bitbucket", "github", "copy"])]
    provider: Option<String>,
    /// Existing projects still reconcile; missing repositories are only reported.
    #[arg(long)]
    existing_only: bool,
    #[arg(long, value_enum, default_value = "plan")]
    operation: Operation,
    #[arg(long)]
    apply: bool,
    /// `settings` only: write to at most this many copies in one run (default 20).
    #[arg(long)]
    max_writes: Option<usize>,
    /// `settings` only: the write gate's kill switch; while it exists no GitHub copy is written.
    #[arg(long)]
    github_kill_switch: Option<PathBuf>,
    /// `settings` only: the write gate's pause directory; `<dir>/<org>` pauses that organisation.
    #[arg(long)]
    github_paused_dir: Option<PathBuf>,
    /// `members` only: the identity table (TOML, person id to a handle per forge).
    #[arg(long)]
    identities: Option<PathBuf>,
    /// `members` only: the report of a `settings` run; only the copies it found are planned for.
    #[arg(long)]
    copies_from: Option<PathBuf>,
    /// `members` only: the login that is never touched (default julian-corbet).
    #[arg(long)]
    operator: Option<String>,
    /// `wiki` only: the SSH deploy key (private half; `<key>.pub` beside it) used to push the copy wikis.
    #[arg(long)]
    wiki_key: Option<PathBuf>,
    /// `wiki` only: the known_hosts file the push verifies the host against.
    #[arg(long)]
    wiki_known_hosts: Option<PathBuf>,
    /// `wiki` only: the id of the deploy key on GitLab (copies without it enable it by id).
    #[arg(long)]
    wiki_key_id: Option<u64>,
    /// `wiki` only: push to at most this many copies in one run (default 20).
    #[arg(long, default_value_t = 20)]
    wiki_max_pushes: usize,
    /// `wiki` only: seconds to wait between two pushes (default 2).
    #[arg(long, default_value_t = 2)]
    wiki_push_gap_secs: u64,
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
    /// Read-only. The one-time adoption of existing push mirrors (design 3.7, R56): every push
    /// mirror of every declared repository is listed as owned (its address is a declared
    /// destination) or foreign, with counts per forge. Holds and exceptions do not exempt a
    /// repository. Ownership is derived, so there is nothing to write.
    Adopt,
    /// Read-only audit of every declared name: the primary against the
    /// declared one, every destination (found by its recorded id) against the
    /// name it must carry. Holds and exceptions do not exempt a repository.
    Names,
    /// R42: bring the settings of every copy (visibility, description, homepage, topics,
    /// default branch, archived state, issues, wiki, CI) in line with its Forgejo primary.
    /// Read-only without `--apply`; a copy that would have to become public is only reported.
    Settings,
    /// R43: the membership the copies need (read or triage, never write), planned from the
    /// primary's members. Read-only until the identity table is decided.
    Members,
    /// R43: is the default branch of every copy held for the mirroring alone? Read-only; needs
    /// `--copies-from` (the report of a `settings` run).
    Protection,
    /// R40: push the Forgejo wiki to every GitLab copy (fast-forward only; a diverged copy is
    /// held). Read-only without `--apply`.
    Wiki,
}

pub fn run(options: Options) -> Result<()> {
    match options.operation {
        Operation::Settings => return settings::run(&options),
        Operation::Members => return members::run(&options),
        Operation::Protection => return protection::run(&options),
        Operation::Wiki => return wiki::run(&options),
        _ => {}
    }
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
        max_writes: None,
        github_kill_switch: None,
        github_paused_dir: None,
        identities: None,
        copies_from: None,
        operator: None,
        wiki_key: None,
        wiki_known_hosts: None,
        wiki_key_id: None,
        wiki_max_pushes: 20,
        wiki_push_gap_secs: 2,
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
        max_writes: None,
        github_kill_switch: None,
        github_paused_dir: None,
        identities: None,
        copies_from: None,
        operator: None,
        wiki_key: None,
        wiki_known_hosts: None,
        wiki_key_id: None,
        wiki_max_pushes: 20,
        wiki_push_gap_secs: 2,
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
            Operation::Plan | Operation::Status | Operation::Names | Operation::Adopt
        )
    {
        return Err("Plan, status, names and adopt cannot apply changes".into());
    }
    let contributed = cfrg::contributed::Contributed::from_placement(&placement, &[])?;
    let contributed_paths = contributed.source_paths();
    // `--provider copy` runs the copy pass alone; it covers every repository the forge lists, declared or not.
    let copy_only = options.provider.as_deref() == Some("copy");
    if !copy_only
        && options.repository.as_ref().is_some_and(|path| {
            !placement.repositories.iter().any(|r| r.path == *path)
            // Forgejo paths are case-insensitive and the contributed list is held in lowercase.
            && !contributed_paths.iter().any(|p| p.eq_ignore_ascii_case(path))
        })
    {
        return Err("Selected repository is not declared".into());
    }
    let mut io = Http::open(&options.state, options.apply)?;
    let mut report = Vec::new();
    let mut incomplete = false;
    if copy_only && placement.copy.is_none() {
        return Err("The placement declares no copy".into());
    }
    let mut tally: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let result = (|| -> Result<()> {
        // Contributed projects (operating model R10, R11) have no mirror, and their copies on our forge run no CI of
        // ours: the Actions unit stays off, or the upstream's own workflows would run on our runners. First, so that
        // no error on an owned repository below can skip it.
        if !copy_only && options.provider.is_none() {
            incomplete |= contributed_actions(
                &mut io,
                &placement.source,
                &contributed_paths,
                options.operation,
                options.apply,
                options.repository.as_deref(),
                &mut report,
            );
        }
        for declared in placement.repositories.iter().filter(|_| !copy_only) {
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
            if options.operation == Operation::Adopt {
                match adopt_one(&mut io, &placement, declared, &mut tally, &mut report) {
                    Ok(()) => {}
                    Err(error) => {
                        report.push(
                            json!({"source":declared.path, "state":"error", "error":error.to_string()}),
                        );
                        incomplete = true;
                    }
                }
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
                // Only an explicit `remote_name` claim keeps a held repository in the pass; `adopt`
                // is what lists the mirrors a held repository has.
                if !declared
                    .destinations
                    .iter()
                    .any(|d| d.remote_name.is_some())
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
            .verify_source(&mut io);
            let current = match current {
                Ok(current) => current,
                Err(error) => {
                    // One repository that Forgejo does not vouch for never hides the others.
                    eprintln!(
                        "{}",
                        json!({"event":"repository-skipped","source":declared.path,"error":error.to_string()})
                    );
                    report.push(
                        json!({"source":declared.path, "state":"error", "error":error.to_string()}),
                    );
                    incomplete = true;
                    continue;
                }
            };
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
            let mut mirrors = match api.list(&mut io) {
                Ok(mirrors) => mirrors,
                Err(error) => {
                    eprintln!(
                        "{}",
                        json!({"event":"repository-skipped","source":declared.path,"error":error.to_string()})
                    );
                    report.push(
                        json!({"source":declared.path, "state":"error", "error":error.to_string()}),
                    );
                    incomplete = true;
                    continue;
                }
            };
            let mut managed = Vec::new();
            for declared_dest in &repo.destinations {
                if options.provider.as_deref().is_some_and(|selected| {
                    selected
                        != match declared_dest.provider {
                            Provider::Gitlab => "gitlab",
                            Provider::Bitbucket => "bitbucket",
                            Provider::Github => "github",
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
                let (mut mirror, owner) = locate(&dest, &mirrors, &known)?;
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
                    // A hold means: do not touch. Only a declared absence, with nothing holding the
                    // repository or the destination, removes an owned mirror.
                    let removable = removable(dest.absent, held);
                    if let Some(value) = &mirror {
                        if removable && options.apply && options.operation == Operation::Reconcile {
                            api.delete(
                                &mut io,
                                value["remote_name"].as_str().ok_or("Missing remote name")?,
                            )?;
                            mirrors.retain(|m| m["remote_name"] != value["remote_name"]);
                        } else {
                            incomplete = true;
                        }
                    }
                    let state = match (&mirror, removable) {
                        (None, true) => "absent",
                        (None, false) => "held",
                        (Some(_), true)
                            if options.apply && options.operation == Operation::Reconcile =>
                        {
                            "absent"
                        }
                        (Some(_), true) => "delete-planned",
                        (Some(_), false) => "held-untouched",
                    };
                    report
                        .push(json!({"source":repo.path, "destination":dest.path, "state":state}));
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
                    Provider::Github => cghb::native::naming(&mut io, &dest, write)?,
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
                    let (found, owner) = locate(&dest, &mirrors, &known)?;
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
                    Provider::Github => cghb::native::ensure_destination_repo(
                        &mut io,
                        repo,
                        &dest,
                        write,
                        allow_create,
                    )?,
                };
                if dest.use_ssh && target.is_some() && mirror.is_none() && write {
                    let value = create_owned(&api, &mut io, &dest, &url)?;
                    mirrors.push(value.clone());
                    mirror = Some(value);
                }
                let lock_exempt = dest.lock_exception.is_some()
                    && dest.provider == Provider::Github
                    && repo.private;
                let protected = if target.is_some() && lock_exempt {
                    report.push(json!({"source":repo.path, "destination":dest.path, "state":"lock-exception", "lock":"unavailable", "reason":dest.lock_exception}));
                    true
                } else if target.is_some() {
                    match dest.provider {
                        Provider::Gitlab if dest.use_ssh => match &mirror {
                            Some(value) => cglb::native::protect_ssh(
                                &mut io,
                                &dest,
                                &repo.default_branch,
                                value,
                                write,
                            )?,
                            None => false,
                        },
                        Provider::Gitlab => {
                            cglb::native::protect(&mut io, &dest, &repo.default_branch, write)?
                        }
                        Provider::Bitbucket => cbkt::native::protect(&mut io, &dest, write)?,
                        Provider::Github => cghb::native::protect(&mut io, repo, &dest, write)?,
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
                        replaced = Some(name.to_string());
                    } else {
                        report.push(json!({"source":repo.path, "destination":dest.path, "state":"replace-planned"}));
                        incomplete = true;
                        continue;
                    }
                }
                if mirror.is_none() && write {
                    let value = create_owned(&api, &mut io, &dest, &url)?;
                    if dest.use_ssh {
                        cglb::native::protect_ssh(
                            &mut io,
                            &dest,
                            &repo.default_branch,
                            &value,
                            true,
                        )?;
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
                        Provider::Github => cghb::native::finalize_default_branch(
                            &mut io, repo, &dest, target, write,
                        )?,
                    };
                    incomplete |= !default_branch_ready;
                    managed.push(value["remote_name"].clone());
                    let mut row = json!({"source":repo.path, "destination":dest.path, "state":"configured", "default_branch_ready":default_branch_ready, "mirror":cfgj::native::status(value)});
                    // A configured mirror is not proof of replication: where the
                    // destination can be read cheaply, compare the refs themselves.
                    if dest.provider == Provider::Github
                        && matches!(
                            options.operation,
                            Operation::Plan | Operation::Reconcile | Operation::SyncNow
                        )
                    {
                        let heads = compare_heads(&mut io, &api, repo, &dest)?;
                        incomplete |= heads["equal"] != true;
                        row["heads"] = heads;
                    }
                    report.push(row);
                } else {
                    incomplete = true;
                    report.push(
                    json!({"source":repo.path, "destination":dest.path, "state":"create-planned"}),
                );
                }
            }
            if options.provider.is_none() {
                // Foreign push mirrors (not a declared destination of this repository) are
                // reported and never touched (design 3.7).
                for mirror in mirrors.iter().filter(|m| {
                    !managed.contains(&m["remote_name"])
                        && !owned_by(repo, placement.copy.as_ref(), m)
                }) {
                    report.push(json!({"source":repo.path, "state":"foreign", "forge":forge_of(mirror), "mirror":cfgj::native::status(mirror)}));
                }
            }
            if options.operation == Operation::SyncNow {
                // This Forgejo endpoint synchronizes every remote; never trigger
                // undeclared, held, absent or unprotected destinations as a side effect.
                let unmanaged: Vec<Value> = mirrors
                    .iter()
                    .filter(|m| !copy::is_copy_mirror(placement.copy.as_ref(), m))
                    .cloned()
                    .collect();
                if !all_mirrors_managed(&unmanaged, &managed) {
                    return Err("Sync-now would include an unmanaged or unready mirror".into());
                }
                if options.apply && !mirrors.is_empty() {
                    api.sync_now(&mut io)?;
                }
            }
        }
        if options.operation == Operation::Adopt {
            for (forge, (owned, foreign)) in &tally {
                report.push(
                    json!({"state":"ownership", "forge":forge, "owned":owned, "foreign":foreign}),
                );
            }
            let (owned, foreign) = tally
                .values()
                .fold((0, 0), |sum, n| (sum.0 + n.0, sum.1 + n.1));
            report.push(json!({"state":"ownership-total", "owned":owned, "foreign":foreign}));
        }
        Ok(())
    })();
    let result = result.and_then(|()| {
        let wanted = options.provider.is_none() || copy_only;
        let Some(copy) = placement.copy.as_ref().filter(|_| wanted) else {
            return Ok(());
        };
        let write = match (options.operation, options.apply) {
            (Operation::Reconcile, true) => true,
            (Operation::Plan | Operation::Status | Operation::Reconcile, false) => false,
            _ => return Ok(()),
        };
        // Our own forge, reached inside the cluster: one read a second (the guard in front of Forgejo
        // limits browsing per address), a short gap between writes, no creation spacing of a provider.
        io.set_pacing(Pacing {
            read: 1,
            write: 2,
            creation: 0,
        });
        let mut ssh;
        let mut none = copy::NoReceiver;
        let receiver: &mut dyn copy::Receiver = if write {
            ssh = copy::Ssh::open(copy)?;
            &mut ssh
        } else {
            &mut none
        };
        let (rows, open) = copy::reconcile(
            copy,
            &placement.source,
            &mut io,
            receiver,
            write,
            options.repository.as_deref(),
        )?;
        report.extend(rows);
        incomplete |= open;
        Ok(())
    });
    Ok((report, incomplete, result))
}

/// The Actions pass over the contributed repositories of the source forge (operating model R10, R11). Reads each one;
/// only `reconcile --apply` switches Actions off. `names`, `rename` and `sync-now` do not concern these repositories
/// and send nothing. `only` selects one repository.
///
/// True when something is open: Actions left on, a repository that is declared but cannot be found (a wrong path, or
/// one this credential cannot see: Forgejo answers 404 for both, and neither is "safe"), or one that failed to answer.
/// One failing repository never stops the others.
fn contributed_actions(
    io: &mut dyn cfrg::native::http::Transport,
    source: &cfrg::native::Endpoint,
    paths: &[String],
    operation: Operation,
    apply: bool,
    only: Option<&str>,
    report: &mut Vec<Value>,
) -> bool {
    if matches!(
        operation,
        Operation::Names | Operation::Rename | Operation::SyncNow
    ) {
        return false;
    }
    let write = apply && operation == Operation::Reconcile;
    let mut open = false;
    for path in paths
        .iter()
        .filter(|path| only.is_none_or(|only| only.eq_ignore_ascii_case(path)))
    {
        match cfgj::native::contributed_actions(io, source, path, write) {
            Ok(state) => {
                open |= matches!(
                    state,
                    cfgj::native::ActionsState::On | cfgj::native::ActionsState::Absent
                );
                report
                    .push(json!({"source":path, "state":"contributed", "actions":state.as_str()}));
            }
            Err(error) => {
                open = true;
                report.push(
                    json!({"source":path, "state":"contributed", "actions":"error", "error":error.to_string()}),
                );
            }
        }
    }
    open
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
            Provider::Github => cghb::native::naming(io, &dest, false)?,
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

/// The Forgejo mirror of a destination under any of its `known` names, and the owner claim
/// the placement itself declares (`remote_name`). Ownership is derived (design 3.7): a push
/// mirror whose address is one of the destination's names is ours; nothing is recorded.
fn locate(
    dest: &Destination,
    mirrors: &[Value],
    known: &[String],
) -> Result<(Option<Value>, Option<String>)> {
    Ok((
        mirror_named(mirrors, dest, known)?,
        dest.remote_name.clone(),
    ))
}

/// A hold means: do not touch. An owned mirror may be removed only when the placement declares
/// the destination absent and nothing holds the repository, the destination or its workspace.
fn removable(absent: bool, held: bool) -> bool {
    absent && !held
}

/// The forge a mirror pushes to: the host of its address, userinfo removed.
fn forge_of(mirror: &Value) -> String {
    mirror["remote_address"]
        .as_str()
        .and_then(|address| address.split_once("://"))
        .and_then(|(_, rest)| rest.split('/').next())
        .and_then(|authority| authority.rsplit('@').next())
        .unwrap_or("unknown")
        .to_string()
}

/// `--operation adopt` for one repository: every push mirror, owned or foreign. Reads only.
fn adopt_one(
    io: &mut dyn Transport,
    placement: &Placement,
    declared: &Repository,
    tally: &mut BTreeMap<String, (usize, usize)>,
    report: &mut Vec<Value>,
) -> Result<()> {
    let api = cfgj::native::Mirrors {
        endpoint: &placement.source,
        repository: declared,
    };
    let current = api.verify_source(io)?;
    let mut followed = declared.clone();
    if current != declared.path {
        report.push(json!({"source":current, "declared":declared.path, "state":"source-renamed"}));
        followed.path = current;
    }
    let repo = &followed;
    let api = cfgj::native::Mirrors {
        endpoint: &placement.source,
        repository: repo,
    };
    let mirrors = api.list(io)?;
    let mut owned = 0;
    for mirror in &mirrors {
        let is_ours = owned_by(repo, placement.copy.as_ref(), mirror);
        let entry = tally.entry(forge_of(mirror)).or_default();
        if is_ours {
            entry.0 += 1;
            owned += 1;
        } else {
            entry.1 += 1;
        }
        report.push(json!({"source":repo.path, "state":if is_ours {"owned"} else {"foreign"}, "forge":forge_of(mirror), "mirror":cfgj::native::status(mirror)}));
    }
    if owned == 0 && mirrors.is_empty() {
        report.push(json!({"source":repo.path, "state":"no-mirrors"}));
    }
    Ok(())
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
        Provider::Github => cghb::native::clone_url(&named),
    }
}

/// The commit the default branch points at on the primary and on the
/// destination, and whether they are the same one.
fn compare_heads(
    io: &mut dyn Transport,
    api: &cfgj::native::Mirrors<'_>,
    repo: &Repository,
    dest: &Destination,
) -> Result<Value> {
    let source = api.head(io, &repo.default_branch)?;
    let destination = cghb::native::head(io, dest, &repo.default_branch)?;
    Ok(json!({
        "branch": repo.default_branch, "source": source, "destination": destination,
        "equal": source.is_some() && source == destination
    }))
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
    fn a_hold_never_lets_a_mirror_be_removed() {
        assert!(removable(true, false));
        // Held and absent, or only held: the mirror stays exactly as it is.
        assert!(!removable(true, true));
        assert!(!removable(false, true));
        assert!(!removable(false, false));
    }

    #[test]
    fn the_forge_of_a_mirror_is_its_host_without_userinfo() {
        assert_eq!(
            forge_of(&mirror("m", "https://user:pw@github.com/org/repo.git")),
            "github.com"
        );
        assert_eq!(forge_of(&json!({"remote_name":"m"})), "unknown");
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

    struct Forge {
        actions: std::collections::BTreeMap<String, bool>,
        seen: Vec<(String, String)>,
    }
    impl cfrg::native::http::Transport for Forge {
        fn send(
            &mut self,
            request: cfrg::native::http::Request,
        ) -> Result<cfrg::native::http::Response> {
            self.seen
                .push((request.method.to_owned(), request.path.clone()));
            let name = request.path.trim_start_matches("/api/v1/repos/").to_owned();
            let Some(state) = self.actions.get_mut(&name) else {
                return Ok(cfrg::native::http::Response {
                    status: 404,
                    body: json!({}),
                });
            };
            if request.method == "PATCH" {
                *state = request
                    .body
                    .as_ref()
                    .and_then(|b| b["has_actions"].as_bool())
                    .unwrap();
            }
            Ok(cfrg::native::http::Response {
                status: 200,
                body: json!({"has_actions": *state}),
            })
        }
    }
    fn forge(on: &[&str], off: &[&str]) -> Forge {
        Forge {
            actions: on
                .iter()
                .map(|p| ((*p).to_owned(), true))
                .chain(off.iter().map(|p| ((*p).to_owned(), false)))
                .collect(),
            seen: Vec::new(),
        }
    }
    fn source() -> cfrg::native::Endpoint {
        cfrg::native::Endpoint {
            origin: "https://forge.example".into(),
            token_env: "T".into(),
        }
    }
    fn paths() -> Vec<String> {
        vec!["me/a".into(), "me/b".into()]
    }
    fn pass(
        io: &mut Forge,
        operation: Operation,
        apply: bool,
        only: Option<&str>,
    ) -> (bool, Vec<Value>) {
        let mut report = Vec::new();
        let open =
            contributed_actions(io, &source(), &paths(), operation, apply, only, &mut report);
        (open, report)
    }
    fn writes(io: &Forge) -> usize {
        io.seen.iter().filter(|(method, _)| method != "GET").count()
    }

    #[test]
    fn a_plan_and_a_status_read_the_actions_switch_and_report_it_as_open() {
        for operation in [Operation::Plan, Operation::Status] {
            let mut io = forge(&["me/a"], &["me/b"]);
            let (left_on, report) = pass(&mut io, operation, false, None);
            assert!(left_on, "Actions that are on make the run incomplete");
            assert_eq!(
                report,
                vec![
                    json!({"source":"me/a","state":"contributed","actions":"on"}),
                    json!({"source":"me/b","state":"contributed","actions":"off"}),
                ]
            );
            assert_eq!(writes(&io), 0);
            assert!(io.actions["me/a"], "a read-only pass changes nothing");
        }
    }

    #[test]
    fn reconcile_without_apply_changes_nothing() {
        let mut io = forge(&["me/a", "me/b"], &[]);
        let (left_on, _) = pass(&mut io, Operation::Reconcile, false, None);
        assert!(left_on);
        assert_eq!(writes(&io), 0);
    }

    #[test]
    fn reconcile_with_apply_switches_actions_off_and_nothing_else() {
        let mut io = forge(&["me/a", "me/b"], &[]);
        let (left_on, report) = pass(&mut io, Operation::Reconcile, true, None);
        assert!(!left_on, "nothing is left on");
        assert!(!io.actions["me/a"]);
        assert!(!io.actions["me/b"]);
        assert_eq!(
            writes(&io),
            2,
            "one PATCH for each repository that had Actions on"
        );
        assert!(
            report.contains(&json!({"source":"me/a","state":"contributed","actions":"disabled"}))
        );
        // Run again: already off, no further write.
        let before = io.seen.len();
        let (left_on, report) = pass(&mut io, Operation::Reconcile, true, None);
        assert!(!left_on);
        assert_eq!(writes(&io), 2);
        assert_eq!(
            io.seen.len() - before,
            2,
            "one read for each repository, nothing more"
        );
        assert!(report.contains(&json!({"source":"me/a","state":"contributed","actions":"off"})));
    }

    #[test]
    fn the_operations_that_do_not_concern_these_repositories_send_nothing() {
        for operation in [Operation::Names, Operation::Rename, Operation::SyncNow] {
            for apply in [false, true] {
                let mut io = forge(&["me/a"], &[]);
                let (left_on, report) = pass(&mut io, operation, apply, None);
                assert!(!left_on && report.is_empty() && io.seen.is_empty());
            }
        }
    }

    #[test]
    fn one_repository_can_be_selected() {
        let mut io = forge(&["me/a", "me/b"], &[]);
        // Forgejo paths are case-insensitive: the selection may be written as the repository is named.
        let (left_on, report) = pass(&mut io, Operation::Reconcile, true, Some("Me/B"));
        assert!(!left_on);
        assert_eq!(
            report,
            vec![json!({"source":"me/b","state":"contributed","actions":"disabled"})]
        );
        assert!(io.actions["me/a"], "the others are left alone");
        let mut io = forge(&["me/a"], &[]);
        let (_, report) = pass(&mut io, Operation::Status, false, Some("me/unlisted"));
        assert!(report.is_empty() && io.seen.is_empty());
    }

    #[test]
    fn a_declared_repository_that_cannot_be_found_leaves_the_run_open() {
        // Forgejo answers 404 for a repository that does not exist and for one this credential cannot see: neither
        // is "safe".
        let mut io = forge(&[], &["me/b"]);
        let mut report = Vec::new();
        let open = contributed_actions(
            &mut io,
            &source(),
            &["me/b".into(), "me/hidden".into()],
            Operation::Reconcile,
            true,
            None,
            &mut report,
        );
        assert!(open);
        assert_eq!(
            report,
            vec![
                json!({"source":"me/b","state":"contributed","actions":"off"}),
                json!({"source":"me/hidden","state":"contributed","actions":"absent"}),
            ]
        );
    }

    #[test]
    fn one_failing_repository_does_not_stop_the_others() {
        struct Flaky(Forge);
        impl cfrg::native::http::Transport for Flaky {
            fn send(
                &mut self,
                request: cfrg::native::http::Request,
            ) -> Result<cfrg::native::http::Response> {
                if request.path.ends_with("me/broken") {
                    return Ok(cfrg::native::http::Response {
                        status: 500,
                        body: json!({}),
                    });
                }
                self.0.send(request)
            }
        }
        let mut io = Flaky(forge(&["me/a"], &[]));
        let mut report = Vec::new();
        let open = contributed_actions(
            &mut io,
            &source(),
            &["me/broken".into(), "me/a".into()],
            Operation::Reconcile,
            true,
            None,
            &mut report,
        );
        assert!(open, "an error leaves the run open");
        assert_eq!(report[0]["actions"], "error");
        assert_eq!(
            report[1],
            json!({"source":"me/a","state":"contributed","actions":"disabled"}),
            "the next repository was still handled"
        );
        assert!(!io.0.actions["me/a"]);
    }
}
