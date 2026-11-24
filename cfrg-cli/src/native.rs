mod copy;
mod creds;
mod derive;
mod members;
mod protection;
mod settings;
mod wiki;

use cfrg::{
    native::{
        http::{Http, Pacing, Transport},
        owned_by, plan_move,
        transfer::{self, Transfer},
        Destination, Move, Naming, Placement, Provider, Repository,
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
    /// The directory of the credential files (`forgejo-token`, `gitlab-token`, `github-token` or the App's `github-app-id` and `github-app-key`, `bitbucket-token`);
    /// a name not found there falls back to the environment variable of the endpoint.
    #[arg(long, env = "CFRG_CREDENTIALS_DIRECTORY")]
    credentials_dir: Option<PathBuf>,
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
    /// `settings` only: the GitHub gate's catalogue (`catalog/github-gate.json`); a GitHub copy is
    /// written only when the placement declares it and the catalogue lists it.
    #[arg(long)]
    github_gate_catalog: Option<PathBuf>,
    /// `members` only: the identity table (TOML, person id to a handle per forge).
    #[arg(long)]
    identities: Option<PathBuf>,
    /// `members` only: the report of a `settings` run; only the copies it found are planned for.
    #[arg(long)]
    copies_from: Option<PathBuf>,
    /// `members` only: the login that is never touched (required).
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
    /// Read-only. Prints the placement as every other operation sees it: the declared rows plus the rows derived from
    /// the `derive` policy, with the `derive-hold` rows on standard error. For a consumer that has to know every repository's copies
    /// (the content replication, the relay's allow list) without a hand list.
    Resolve,
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
        Operation::Resolve => return resolve(&options),
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
/// `--operation resolve`: the declared and the derived rows as one placement document on standard output.
fn resolve(options: &Options) -> Result<()> {
    if options.apply {
        return Err("Resolve is read-only".into());
    }
    let creds = creds::Creds::new(options.credentials_dir.clone());
    let mut placement = creds.placement(&fs::read(&options.placement)?)?;
    placement.validate()?;
    let mut io = creds.open(&options.state, false)?;
    for row in derive::apply(
        &creds,
        &mut placement,
        &mut io,
        options.repository.as_deref(),
    )? {
        if row["state"] == "derive-hold" {
            eprintln!("{row}");
        }
    }
    placement.validate()?;
    println!("{}", serde_json::to_string(&placement)?);
    Ok(())
}

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
        github_gate_catalog: None,
        identities: None,
        copies_from: None,
        operator: None,
        wiki_key: None,
        wiki_known_hosts: None,
        wiki_key_id: None,
        credentials_dir: None,
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
        github_gate_catalog: None,
        identities: None,
        copies_from: None,
        operator: None,
        wiki_key: None,
        wiki_known_hosts: None,
        wiki_key_id: None,
        credentials_dir: None,
        wiki_max_pushes: 20,
        wiki_push_gap_secs: 2,
    };
    let (report, incomplete, result) = execute(&options)?;
    result?;
    Ok(json!({"complete": !incomplete, "repositories": report}))
}

fn execute(options: &Options) -> Result<(Vec<Value>, bool, Result<()>)> {
    let creds = creds::Creds::new(options.credentials_dir.clone());
    let mut placement = creds.placement(&fs::read(&options.placement)?)?;
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
    let mut io = creds.open(&options.state, options.apply)?;
    // `--provider copy` runs the copy pass alone; it covers every repository the forge lists, declared or not.
    let copy_only = options.provider.as_deref() == Some("copy");
    let mut report = Vec::new();
    // The derived default copies of every listed repository that is not declared (placement `derive`).
    if !copy_only {
        report.extend(derive::apply(
            &creds,
            &mut placement,
            &mut io,
            options.repository.as_deref(),
        )?);
        placement.validate()?;
    }
    if !copy_only
        && options.repository.as_ref().is_some_and(|path| {
            !placement.repositories.iter().any(|r| r.path == *path)
            // Forgejo paths are case-insensitive and the contributed list is held in lowercase.
            && !contributed_paths.iter().any(|p| p.eq_ignore_ascii_case(path))
        })
    {
        return Err("Selected repository is not declared".into());
    }
    let mut incomplete = false;
    if copy_only && placement.copy.is_none() {
        return Err("The placement declares no copy".into());
    }
    let mut tally: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    // GitHub transfers: ONE per run, whatever it ends in (the pacing policy); the next one waits in the state file.
    let mut github_transfer_spent = false;
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
            let source_held = !placement.mirrorable(declared);
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
                // An LFS repository's destination that is not itself a pointers-only SSH mirror carries real content (the content
                // replication's): native leaves it alone.
                if !placement.destination_mirrorable(repo, declared_dest) {
                    continue;
                }
                let outcome: Result<()> = (|| -> Result<()> {
                    // Where this destination must live now, and every name it may
                    // still be known under (the file's last record, the live name).
                    let mut dest = declared_dest.clone();
                    dest.path = repo.destination_path(declared_dest);
                    let url = destination_url(&dest, &dest.path);
                    // A namespace change the placement declares (`org_map`) is a transfer. Bitbucket's is a sequence of passes that
                    // keeps the old mirror beside a second one, so the ordinary mirror logic below must not see it until it is done.
                    let may_transfer =
                        matches!(options.operation, Operation::Plan | Operation::Reconcile);
                    let create_ok =
                        !options.existing_only && options.operation != Operation::Rename;
                    if dest.provider == Provider::Bitbucket
                        && may_transfer
                        && repo.transfer_declared(Provider::Bitbucket)
                        && !dest.absent
                        && dest.hold.is_none()
                        && !source_held
                        && dest
                            .repository_id
                            .as_deref()
                            .is_none_or(|id| io.recorded("bitbucket-transfer-by-new", id).is_none())
                    {
                        if let (Some(old_uuid), Some(old_ws)) = (
                            dest.repository_id.clone(),
                            old_workspace(&dest, &declared_dest.recorded_path, &mirrors),
                        ) {
                            let bb =
                                bitbucket_creation_dest(&dest, repo, placement.derive.as_ref());
                            let moving = options.apply && options.operation == Operation::Reconcile;
                            match bitbucket_move(
                                &mut io,
                                &api,
                                repo,
                                &dest,
                                &bb,
                                &old_uuid,
                                &old_ws,
                                &mut mirrors,
                                moving,
                                create_ok,
                                &mut report,
                            )? {
                                MoveStep::Stop => {
                                    incomplete = true;
                                    return Ok(());
                                }
                                MoveStep::Continue(new_uuid) => dest.repository_id = Some(new_uuid),
                            }
                        }
                    }
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
                            if removable
                                && options.apply
                                && options.operation == Operation::Reconcile
                            {
                                let remote =
                                    value["remote_name"].as_str().ok_or("Missing remote name")?;
                                match delete_mirror(&mut io, &api, &placement.source, repo, remote)?
                                {
                                    Deleted::Yes => {
                                        mirrors
                                            .retain(|m| m["remote_name"] != value["remote_name"]);
                                    }
                                    Deleted::Held(reason) => {
                                        // One refused deletion must not stop the pass: record it and go on.
                                        incomplete = true;
                                        report.push(json!({"source":repo.path, "destination":dest.path, "state":"delete-held", "reason":reason}));
                                        return Ok(());
                                    }
                                }
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
                        report.push(
                            json!({"source":repo.path, "destination":dest.path, "state":state}),
                        );
                        return Ok(());
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
                        return Ok(());
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
                            return Ok(());
                        }
                        Naming::Blocked { from, reason }
                            if plan_move(&from, &dest.path) != Move::Transfer =>
                        {
                            report.push(json!({"source":repo.path, "destination":dest.path, "state":"rename-blocked", "from":from, "reason":reason}));
                            incomplete = true;
                            return Ok(());
                        }
                        // The namespace differs: the destination is moved by its provider's means (capability `transfer`),
                        // and only when the placement declares the move (`org_map`), under `plan` (read-only) or `reconcile`.
                        Naming::Blocked { from, .. } => {
                            let moving = options.apply && options.operation == Operation::Reconcile;
                            let needed = |why: &str| json!({"source":repo.path, "destination":dest.path, "state":"transfer-needed", "from":from, "reason":why});
                            if !repo.transfer_declared(dest.provider) {
                                report.push(needed("the placement's org_map declares no namespace for this organisation on this provider; nothing moves implicitly"));
                                incomplete = true;
                                return Ok(());
                            }
                            if !matches!(options.operation, Operation::Plan | Operation::Reconcile)
                                || dest.provider == Provider::Bitbucket
                            {
                                report
                                    .push(needed("a transfer runs under plan and reconcile only"));
                                incomplete = true;
                                return Ok(());
                            }
                            if dest.provider == Provider::Github && moving {
                                if github_transfer_spent {
                                    report.push(json!({"source":repo.path, "destination":dest.path, "state":"transfer-blocked", "from":from, "reason":"one GitHub transfer per run"}));
                                    incomplete = true;
                                    return Ok(());
                                }
                                github_transfer_spent = true;
                            }
                            let outcome = transfer_one(&mut io, options, &dest, &from, moving)?;
                            match outcome {
                                Transfer::Planned { from, steps } => {
                                    report.push(json!({"source":repo.path, "destination":dest.path, "state":"transfer-planned", "from":from, "steps":steps}));
                                    incomplete = true;
                                    return Ok(());
                                }
                                Transfer::Pending {
                                    from,
                                    detail,
                                    new_repository_id,
                                } => {
                                    report.push(json!({"source":repo.path, "destination":dest.path, "state":"transfer-pending", "from":from, "detail":detail, "new_repository_id":new_repository_id}));
                                    incomplete = true;
                                    return Ok(());
                                }
                                Transfer::Blocked { from, reason } => {
                                    report.push(json!({"source":repo.path, "destination":dest.path, "state":"transfer-blocked", "from":from, "reason":reason}));
                                    incomplete = true;
                                    return Ok(());
                                }
                                Transfer::Done { from, detail } => {
                                    report.push(json!({"source":repo.path, "destination":dest.path, "state":"transferred", "from":from, "detail":detail}));
                                    Some(from)
                                }
                            }
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
                            return Ok(());
                        }
                    }
                    // A rename never creates: a destination that is missing is reported.
                    let allow_create =
                        !options.existing_only && options.operation != Operation::Rename;
                    let target = match dest.provider {
                        Provider::Gitlab => cglb::native::ensure_destination_repo(
                            &mut io,
                            repo,
                            &dest,
                            write,
                            allow_create,
                        )?,
                        Provider::Bitbucket => {
                            let bb =
                                bitbucket_creation_dest(&dest, repo, placement.derive.as_ref());
                            let found = cbkt::native::ensure_destination_repo(
                                &mut io, repo, &bb, write, false,
                            )?;
                            let action = bitbucket_place(
                                &mut io,
                                &bb,
                                repo,
                                found.as_ref(),
                                write && allow_create,
                            )?;
                            if action != "in-place" {
                                eprintln!(
                                    "{}",
                                    json!({"event": "bitbucket-project", "destination": bb.path, "project": bb.namespace, "action": action})
                                );
                            }
                            match found {
                                Some(value) => Some(value),
                                None => cbkt::native::ensure_destination_repo(
                                    &mut io,
                                    repo,
                                    &bb,
                                    write,
                                    allow_create,
                                )?,
                            }
                        }
                        Provider::Github => cghb::native::ensure_destination_repo(
                            &mut io,
                            repo,
                            &dest,
                            write,
                            allow_create,
                        )?,
                    };
                    // A GitLab copy that carries pointers only must have LFS switched OFF: with LFS on, GitLab rejects every push whose
                    // pointers name objects the project does not hold ("LFS objects are missing"). Derived from external_content, and
                    // corrected on an existing copy too.
                    if write
                        && dest.provider == Provider::Gitlab
                        && placement.pointers_only_on(repo, Provider::Gitlab)
                    {
                        if let Some(project) = &target {
                            gitlab_lfs_off(&mut io, &dest, project)?;
                        }
                    }
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
                            Provider::Bitbucket if dest.use_ssh => match &mirror {
                                // The key of the Forgejo SSH push mirror lives on the mirror account; restrictions admit that user.
                                Some(value) => {
                                    cbkt::native::enroll_key(&mut io, &dest, value, write)?
                                        && cbkt::native::restrict(
                                            &mut io,
                                            &dest,
                                            write,
                                            &[dest.restriction_user()],
                                        )?
                                }
                                None => false,
                            },
                            Provider::Bitbucket => cbkt::native::restrict(
                                &mut io,
                                &dest,
                                write,
                                &[dest.restriction_user()],
                            )?,
                            Provider::Github => cghb::native::protect(&mut io, repo, &dest, write)?,
                        }
                    } else {
                        false
                    };
                    if !protected {
                        report.push(json!({"source":repo.path, "destination":dest.path, "state":"destination-or-protection-required"}));
                        incomplete = true;
                        return Ok(());
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
                            return Ok(());
                        }
                    }
                    if mirror.is_none() && write {
                        let value = create_owned(&api, &mut io, &dest, &url)?;
                        if dest.use_ssh && dest.provider == Provider::Gitlab {
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
                        if dest.use_ssh && dest.provider == Provider::Bitbucket {
                            cbkt::native::enroll_key(&mut io, &dest, &value, true)?;
                            if let Some(old) = &replaced {
                                cbkt::native::forget_key(&mut io, &dest, old)?;
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
                    Ok(())
                })();
                if let Err(error) = outcome {
                    let message = error.to_string();
                    // A rate window, an auth refusal, a server error or an uncertain write stops the run (the transport recorded why);
                    // any other refusal belongs to this one destination: record it with the provider's message and go on.
                    if stops_the_run(&message) {
                        return Err(error);
                    }
                    let destination = repo.destination_path(declared_dest);
                    let flagged = declared_dest.provider == Provider::Github
                        && is_flagged_org(options.github_gate_catalog.as_deref(), &destination);
                    let (row, failed) = failure_row(&message, flagged, &repo.path, &destination);
                    incomplete |= failed;
                    report.push(row);
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
            Naming::Blocked { from, .. } if plan_move(&from, &dest.path) == Move::Transfer => {
                row("transfer-needed", json!({"from":from, "wanted":dest.path}))
            }
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
/// One step of moving a GitLab or GitHub `dest` from `from` into its new namespace. `write` is true only under
/// `reconcile --apply`. GitHub fails closed: a write needs the write gate's kill switch, pause directory AND the gate catalogue
/// (the flagged organisations), is paced three minutes apart through the state file (reported, never slept on), and a transfer
/// requested earlier is read back, never requested again.
fn transfer_one(
    io: &mut Http,
    options: &Options,
    dest: &Destination,
    from: &str,
    write: bool,
) -> Result<Transfer> {
    match dest.provider {
        Provider::Gitlab => transfer::gitlab(io, dest, from, write),
        Provider::Github => {
            let new_owner = dest.path.split('/').next().unwrap_or_default();
            let old_owner = from.split('/').next().unwrap_or_default();
            let id = dest.repository_id.clone().unwrap_or_default();
            let requested = io.state.github_transfers.contains_key(&id);
            let due = io.state.next_github_transfer;
            let refusal = if write
                && (options.github_kill_switch.is_none()
                    || options.github_paused_dir.is_none()
                    || options.github_gate_catalog.is_none())
            {
                Some("the write gate's files were not given (--github-kill-switch, --github-paused-dir, --github-gate-catalog)".to_string())
            } else if options
                .github_kill_switch
                .as_ref()
                .is_some_and(|p| p.exists())
            {
                Some("gate-kill-switch".into())
            } else if [old_owner, new_owner].iter().any(|org| {
                options
                    .github_paused_dir
                    .as_ref()
                    .is_some_and(|dir| dir.join(org).exists())
            }) {
                Some("gate-paused".into())
            } else if [from, dest.path.as_str()]
                .iter()
                .any(|path| is_flagged_org(options.github_gate_catalog.as_deref(), path))
            {
                Some("flagged-organisation".into())
            } else if write && !requested && cfrg::native::http::now()? < due {
                Some(format!(
                    "paced: the next GitHub transfer is due at {due} (unix time)"
                ))
            } else {
                None
            };
            transfer::github(
                io,
                dest,
                from,
                write,
                &transfer::Gate { refusal },
                requested,
            )
        }
        Provider::Bitbucket => Err("a Bitbucket move runs through bitbucket_move".into()),
    }
}

enum MoveStep {
    /// The destination is not finished moving; the rest of the pass leaves it alone.
    Stop,
    /// The move is complete: the rest of the pass addresses the repository under this new uuid.
    Continue(String),
}

/// The workspace a Bitbucket destination moved away from: the last recorded name's, else the one of the owned mirror's address.
fn old_workspace(dest: &Destination, recorded: &str, mirrors: &[Value]) -> Option<String> {
    let new_ws = dest.path.split('/').next()?;
    let from_record = recorded.split('/').next().filter(|ws| !ws.is_empty());
    let from_mirror = dest.remote_name.as_ref().and_then(|name| {
        let mirror = mirrors.iter().find(|m| m["remote_name"] == *name)?;
        let address = mirror["remote_address"].as_str()?;
        let (_, rest) = address.split_once("://")?;
        rest.split('/').nth(1).map(str::to_string)
    });
    from_record
        .map(str::to_string)
        .into_iter()
        .chain(from_mirror)
        .find(|ws| !ws.eq_ignore_ascii_case(new_ws))
}

/// One pass of a Bitbucket move (see `transfer::bitbucket`). While the new repository fills, a SECOND owned mirror pushes to it
/// and the old mirror keeps running, so the two stay comparable; the old mirror is dropped only once the old repository is deleted.
#[allow(clippy::too_many_arguments)]
fn bitbucket_move(
    io: &mut dyn Transport,
    api: &cfgj::native::Mirrors<'_>,
    repo: &Repository,
    dest: &Destination,
    bb: &Destination,
    old_uuid: &str,
    old_ws: &str,
    mirrors: &mut Vec<Value>,
    write: bool,
    allow_create: bool,
    report: &mut Vec<Value>,
) -> Result<MoveStep> {
    let row = |state: &str, extra: Value| {
        let mut row = json!({"source":repo.path, "destination":dest.path, "state":state});
        for (key, value) in extra.as_object().into_iter().flatten() {
            row[key] = value.clone();
        }
        row
    };
    let known_new = io.recorded("bitbucket-transfer", old_uuid);
    let outcome = transfer::bitbucket(
        io,
        bb,
        &transfer::BitbucketMove {
            old_workspace: old_ws,
            old_uuid,
            project_key: &bb.namespace,
            private: repo.private,
            allow_create,
            known_new: known_new.as_deref(),
        },
        write,
    )?;
    match outcome {
        Transfer::Planned { from, steps } => {
            report.push(row("transfer-planned", json!({"from":from, "steps":steps})));
            Ok(MoveStep::Stop)
        }
        Transfer::Blocked { from, reason } => {
            report.push(row(
                "transfer-blocked",
                json!({"from":from, "reason":reason}),
            ));
            Ok(MoveStep::Stop)
        }
        Transfer::Pending {
            from,
            detail,
            new_repository_id,
        } => {
            if write {
                if let Some(new_uuid) = &new_repository_id {
                    io.note("bitbucket-transfer", old_uuid, new_uuid)?;
                    // The second owned mirror, to the new repository, with the destination protection of the new one.
                    let mut new_dest = bb.clone();
                    new_dest.repository_id = Some(new_uuid.clone());
                    // The new repository gets what every destination gets (pipelines off, identity verified) before any push.
                    cbkt::native::ensure_destination_repo(io, repo, &new_dest, true, false)?;
                    let url = destination_url(&new_dest, &new_dest.path);
                    if !mirrors
                        .iter()
                        .any(|m| cfgj::native::address_matches(m, &url))
                    {
                        let value = create_owned(api, io, &new_dest, &url)?;
                        if new_dest.use_ssh {
                            cbkt::native::enroll_key(io, &new_dest, &value, true)?;
                        }
                        mirrors.push(value);
                    }
                    let user = new_dest.restriction_user().to_string();
                    cbkt::native::restrict(io, &new_dest, true, &[user.as_str()])?;
                }
            }
            report.push(row(
                "transfer-pending",
                json!({"from":from, "detail":detail, "new_repository_id":new_repository_id}),
            ));
            Ok(MoveStep::Stop)
        }
        Transfer::Done { from, detail } => {
            if !write {
                report.push(row("transfer-planned", json!({"from":from, "steps":["the move is complete; apply records the new uuid and drops the old mirror"]})));
                return Ok(MoveStep::Stop);
            }
            let new_uuid = detail["repository_id"]
                .as_str()
                .ok_or("The finished move names no new repository uuid")?
                .to_string();
            io.note("bitbucket-transfer", old_uuid, &new_uuid)?;
            // The old repository is gone: its mirror goes too (the one that still points at the old name).
            let old_url = destination_url(
                dest,
                &format!(
                    "{old_ws}/{}",
                    dest.path.rsplit('/').next().unwrap_or_default()
                ),
            );
            let new_url = destination_url(dest, &dest.path);
            let stale: Vec<Value> = mirrors
                .iter()
                .filter(|m| {
                    cfgj::native::address_matches(m, &old_url)
                        && !cfgj::native::address_matches(m, &new_url)
                })
                .cloned()
                .collect();
            for old in stale {
                let name = old["remote_name"]
                    .as_str()
                    .ok_or("Missing remote name")?
                    .to_string();
                api.delete(io, &name)?;
                mirrors.retain(|m| m["remote_name"] != old["remote_name"]);
                if dest.use_ssh {
                    cbkt::native::forget_key(io, dest, &name)?;
                }
            }
            report.push(row("transferred", json!({"from":from, "detail":detail})));
            Ok(MoveStep::Continue(new_uuid))
        }
    }
}

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
    io: &mut dyn Transport,
    dest: &cfrg::native::Destination,
    url: &str,
) -> Result<Value> {
    let password = if dest.use_ssh {
        if dest.provider == cfrg::native::Provider::Gitlab {
            cglb::native::deny_branch_writes(io, dest)?;
        }
        String::new()
    } else {
        let value = io
            .credential(&dest.password_env, &dest.path)
            .ok_or("Missing mirror credential environment reference")?;
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("Invalid mirror credential format".into());
        }
        value
    };
    let value = match api.create(io, dest, url, &password) {
        // Forgejo answers HTTP 423 (Locked) to every write on an ARCHIVED repository, push mirrors included: the archive is
        // lifted for the one creation and put back (also when the creation fails), like `delete_mirror` does.
        Err(error) if error.to_string().contains("HTTP 423") => {
            with_archive_lifted(io, api.endpoint, &api.repository.path, |io| {
                api.create(io, dest, url, &password)
            })?
        }
        other => other?,
    };
    if !cfgj::native::matches(&value, dest, url) {
        return Err("Created mirror does not match placement; inspect before retry".into());
    }
    let name = value["remote_name"]
        .as_str()
        .ok_or("Missing created mirror remote name")?;
    cfrg::native::component(name)?;
    Ok(value)
}

/// Errors that must stop the whole run: the transport's own stops (rate window, auth, server error, uncertain write).
fn stops_the_run(message: &str) -> bool {
    message.contains("stopped without retry")
        || message.contains("Uncertain prior write")
        || message.contains("interrupted")
}

/// The destination a Bitbucket repository is created from: a public primary goes to the derive policy's public project, a
/// private one keeps the owner's. A project never holds both kinds.
fn bitbucket_creation_dest(
    dest: &cfrg::native::Destination,
    repo: &Repository,
    derive: Option<&cfrg::native::derive::Derive>,
) -> cfrg::native::Destination {
    let mut dest = dest.clone();
    let wanted = derive.and_then(|d| {
        if repo.private {
            d.bitbucket_private_namespace.as_ref()
        } else {
            d.bitbucket_public_namespace.as_ref()
        }
    });
    if let Some(namespace) = wanted {
        dest.namespace = namespace.clone();
    }
    dest
}

/// Switches Git LFS off on a pointers-only GitLab copy when the project (as GitLab answered it) has it on.
fn gitlab_lfs_off(
    io: &mut dyn Transport,
    dest: &cfrg::native::Destination,
    project: &Value,
) -> Result<()> {
    if project["lfs_enabled"] != json!(true) {
        return Ok(());
    }
    let id = project["id"]
        .as_u64()
        .ok_or("GitLab project without an id")?;
    let done = io.send(cfrg::native::http::Request {
        endpoint: dest.endpoint.clone(),
        auth: cfrg::native::http::Auth::PrivateToken,
        method: "PUT",
        path: format!("/api/v4/projects/{id}"),
        body: Some(json!({"lfs_enabled": false})),
        scope: format!("{}/api", dest.endpoint.origin),
        creation: false,
    })?;
    if done.status != 200 {
        return Err(format!(
            "GitLab refused to switch LFS off on {} (HTTP {})",
            dest.path, done.status
        )
        .into());
    }
    eprintln!(
        "{}",
        json!({"event": "gitlab-lfs-off", "destination": dest.path})
    );
    Ok(())
}

/// Makes sure the Bitbucket project `dest.namespace` exists in the workspace (created on demand, private exactly when the
/// repository is) and, for a copy that already exists, that it sits in it. Going PUBLIC moves the copy first (a public
/// project may hold a private repository); going PRIVATE moves it only once the copy itself is private (a private project
/// refuses a public repository), so the visibility change comes first and the move follows on the next pass.
/// Returns what it did, for the report.
fn bitbucket_place(
    io: &mut dyn Transport,
    dest: &cfrg::native::Destination,
    repo: &Repository,
    existing: Option<&Value>,
    write: bool,
) -> Result<&'static str> {
    let workspace = dest.path.split('/').next().ok_or("Missing workspace")?;
    let request =
        |method: &'static str, path: String, body: Option<Value>| cfrg::native::http::Request {
            endpoint: dest.endpoint.clone(),
            auth: cfrg::native::http::Auth::Bearer,
            method,
            path,
            body,
            scope: format!("{}/workspace/{workspace}", dest.endpoint.origin),
            creation: false,
        };
    if let Some(value) = existing {
        let current = value["project"]["key"].as_str().unwrap_or_default();
        if current == dest.namespace {
            return Ok("in-place");
        }
        if repo.private && value["is_private"] != json!(true) {
            return Ok("held-until-private");
        }
        if !write {
            return Ok("move-planned");
        }
    } else if !write {
        return Ok("project-planned");
    }
    let key = &dest.namespace;
    let seen = io.send(request(
        "GET",
        format!("/2.0/workspaces/{workspace}/projects/{key}"),
        None,
    ))?;
    if seen.status == 404 {
        let made = io.send(request(
            "POST",
            format!("/2.0/workspaces/{workspace}/projects"),
            Some(json!({"name": key, "key": key, "is_private": repo.private})),
        ))?;
        if !matches!(made.status, 200 | 201) {
            return Err(format!(
                "Bitbucket project {key} in {workspace}: HTTP {}",
                made.status
            )
            .into());
        }
    } else if seen.status != 200 {
        return Err(format!(
            "Bitbucket project {key} in {workspace}: HTTP {}",
            seen.status
        )
        .into());
    }
    if existing.is_some() {
        let moved = io.send(request(
            "PUT",
            format!("/2.0/repositories/{}", dest.path),
            Some(json!({"project": {"key": key}})),
        ))?;
        if moved.status != 200 {
            return Err(format!(
                "moving {} to project {key}: HTTP {}",
                dest.path, moved.status
            )
            .into());
        }
        return Ok("moved");
    }
    Ok("project-ready")
}

/// Runs `operation` on an archived Forgejo repository with the archive lifted, and puts the archive back afterwards, also when the
/// operation fails. A repository that cannot be unarchived is an error naming it; so is one that cannot be archived again.
fn with_archive_lifted<T>(
    io: &mut dyn Transport,
    endpoint: &cfrg::native::Endpoint,
    path: &str,
    operation: impl FnOnce(&mut dyn Transport) -> Result<T>,
) -> Result<T> {
    let archive = |io: &mut dyn Transport, archived: bool| -> Result<u16> {
        Ok(io
            .send(cfrg::native::http::Request {
                endpoint: endpoint.clone(),
                auth: cfrg::native::http::Auth::Token,
                method: "PATCH",
                path: format!("/api/v1/repos/{path}"),
                body: Some(json!({"archived": archived})),
                scope: format!("{}/api", endpoint.origin),
                creation: false,
            })?
            .status)
    };
    let status = archive(io, false)?;
    if status != 200 {
        return Err(format!("{path} is archived; unarchiving answered HTTP {status}").into());
    }
    let result = operation(io);
    let restored = archive(io, true)?;
    if restored != 200 {
        return Err(format!(
            "{path} could not be archived again (HTTP {restored}); archive it by hand"
        )
        .into());
    }
    result
}

enum Deleted {
    Yes,
    Held(String),
}

/// Deletes one push mirror. Forgejo answers HTTP 423 (Locked) to every write on an ARCHIVED repository, so the
/// archive is lifted for the call and put back afterwards (also when the deletion fails). Any other refusal of
/// the deletion, or a repository that cannot be unarchived, is a hold for that one mirror, not an error of the step.
fn delete_mirror(
    io: &mut dyn Transport,
    api: &cfgj::native::Mirrors,
    endpoint: &cfrg::native::Endpoint,
    repo: &Repository,
    remote: &str,
) -> Result<Deleted> {
    let error = match api.delete(io, remote) {
        Ok(()) => return Ok(Deleted::Yes),
        Err(error) => error.to_string(),
    };
    if !error.contains("HTTP 423") {
        return Ok(Deleted::Held(error));
    }
    let archive = |io: &mut dyn Transport, archived: bool| -> Result<u16> {
        Ok(io
            .send(cfrg::native::http::Request {
                endpoint: endpoint.clone(),
                auth: cfrg::native::http::Auth::Token,
                method: "PATCH",
                path: format!("/api/v1/repos/{}", repo.path),
                body: Some(json!({"archived":archived})),
                scope: format!("{}/api", endpoint.origin),
                creation: false,
            })?
            .status)
    };
    let status = archive(io, false)?;
    if status != 200 {
        return Ok(Deleted::Held(format!(
            "archived repository; unarchiving answered HTTP {status}"
        )));
    }
    let result = api.delete(io, remote);
    let restored = archive(io, true)?;
    if restored != 200 {
        return Err(format!(
            "{} could not be archived again (HTTP {restored}); archive it by hand",
            repo.path
        )
        .into());
    }
    Ok(match result {
        Ok(()) => Deleted::Yes,
        Err(error) => Deleted::Held(error.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_permission_refusal_does_not_stop_the_run() {
        let refusal = format!(
            "Native API HTTP 403; {}; the run goes on",
            cfrg::native::http::REFUSED_MARK
        );
        assert!(!stops_the_run(&refusal));
        assert!(stops_the_run(
            "Native API HTTP 403; stopped without retry; window/intent recorded"
        ));
    }
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

    fn http(dir: &std::path::Path) -> Http {
        Http::open(&dir.join("state.json"), true).unwrap()
    }

    fn transfer_options(
        kill: Option<PathBuf>,
        paused: Option<PathBuf>,
        catalog: Option<PathBuf>,
    ) -> Options {
        Options {
            placement: PathBuf::new(),
            state: PathBuf::new(),
            repository: None,
            provider: None,
            existing_only: true,
            operation: Operation::Reconcile,
            apply: true,
            max_writes: None,
            github_kill_switch: kill,
            github_paused_dir: paused,
            github_gate_catalog: catalog,
            identities: None,
            copies_from: None,
            operator: None,
            wiki_key: None,
            wiki_known_hosts: None,
            wiki_key_id: None,
            credentials_dir: None,
            wiki_max_pushes: 20,
            wiki_push_gap_secs: 2,
        }
    }

    fn github_destination() -> Destination {
        let json = r#"{"provider":"github","endpoint":{"origin":"https://api.github.com","token_env":"T"},"path":"new-org/app","namespace":"new-org","repository_id":"5","mirror_user":"9","password_env":"T","interval_seconds":3600}"#;
        let repo: Repository = serde_json::from_str(&format!(
            r#"{{"path":"new-org/app","source_id":5,"private":true,"default_branch":"main","content":"native-git","destinations":[{json}]}}"#
        ))
        .unwrap();
        repo.destinations[0].clone()
    }

    #[test]
    fn a_github_transfer_without_the_gates_files_makes_no_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut io = http(dir.path());
        let dest = github_destination();
        // The catalogue is required for a write: kill switch and pause directory alone do not open the gate.
        let paused = dir.path().join("paused");
        std::fs::create_dir(&paused).unwrap();
        for options in [
            transfer_options(None, None, None),
            transfer_options(Some(dir.path().join("kill")), Some(paused), None),
        ] {
            let outcome = transfer_one(&mut io, &options, &dest, "old-org/app", true).unwrap();
            assert!(
                matches!(&outcome, Transfer::Blocked { reason, .. } if reason.contains("--github-gate-catalog")),
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn a_github_transfer_waits_for_a_kill_switch_a_paused_organisation_and_its_pacing() {
        let dir = tempfile::tempdir().unwrap();
        let dest = github_destination();
        let kill = dir.path().join("kill");
        let paused = dir.path().join("paused");
        let catalog = dir.path().join("gate-config.json");
        std::fs::create_dir(&paused).unwrap();
        std::fs::write(&catalog, r#"{"orgs":{}}"#).unwrap();
        std::fs::write(&kill, "").unwrap();
        let mut io = http(dir.path());
        let options = transfer_options(Some(kill.clone()), Some(paused.clone()), Some(catalog));
        let outcome = transfer_one(&mut io, &options, &dest, "old-org/app", true).unwrap();
        assert!(
            matches!(&outcome, Transfer::Blocked { reason, .. } if reason.contains("gate-kill-switch"))
        );
        std::fs::remove_file(&kill).unwrap();
        std::fs::write(paused.join("old-org"), "").unwrap();
        let outcome = transfer_one(&mut io, &options, &dest, "old-org/app", true).unwrap();
        assert!(
            matches!(&outcome, Transfer::Blocked { reason, .. } if reason.contains("gate-paused"))
        );
        std::fs::remove_file(paused.join("old-org")).unwrap();
        // A transfer that is not yet due is reported, never slept on.
        io.state.next_github_transfer = cfrg::native::http::now().unwrap() + 170;
        let outcome = transfer_one(&mut io, &options, &dest, "old-org/app", true).unwrap();
        assert!(
            matches!(&outcome, Transfer::Blocked { reason, .. } if reason.contains("paced")),
            "{outcome:?}"
        );
    }

    /// A tiny world: Forgejo's push mirrors, and Bitbucket's old and new repositories. Everything goes through the transport trait.
    struct World {
        mirrors: Vec<Value>,
        counter: u32,
        old_alive: bool,
        new_alive: bool,
        old_refs: Vec<(String, String)>,
        new_refs: Vec<(String, String)>,
        writes: Vec<String>,
        notes: BTreeMap<String, String>,
        restrictions: Vec<Value>,
    }

    fn r_body(_existing: &[Value], body: &Value) -> Value {
        body.clone()
    }

    const OLD_REPO: &str = "/2.0/repositories/corbet-web/%7Bold%7D";
    const NEW_REPO: &str = "/2.0/repositories/corbet-archv/corbet.ch";

    impl World {
        fn refs(list: &[(String, String)]) -> Value {
            json!({"values": list.iter().map(|(n, h)| json!({"type":"branch","name":n,"target":{"hash":h}})).collect::<Vec<_>>()})
        }
    }

    impl Transport for World {
        fn send(&mut self, r: cfrg::native::http::Request) -> Result<cfrg::native::http::Response> {
            use cfrg::native::http::Response;
            let ok = |status: u16, body: Value| Ok(Response { status, body });
            let (method, path) = (r.method, r.path.as_str());
            let req_body = r.body.clone().unwrap_or(Value::Null);
            if method != "GET" {
                self.writes.push(format!("{method} {path}"));
            }
            if path.contains("/push_mirrors") {
                return match method {
                    "GET" if path.contains("page=1") => ok(200, json!(self.mirrors.clone())),
                    "GET" => ok(200, json!([])),
                    "POST" => {
                        self.counter += 1;
                        let mut mirror = r.body.clone().unwrap();
                        mirror["remote_name"] = json!(format!("remote_mirror_{}", self.counter));
                        self.mirrors.push(mirror.clone());
                        ok(200, mirror)
                    }
                    "DELETE" => {
                        let name = path.rsplit('/').next().unwrap().to_string();
                        self.mirrors.retain(|m| m["remote_name"] != name.as_str());
                        ok(204, Value::Null)
                    }
                    _ => panic!("{method} {path}"),
                };
            }
            if let Some(rest) = path.strip_prefix(OLD_REPO) {
                return match (method, rest) {
                    _ if !self.old_alive => ok(404, Value::Null),
                    ("GET", "") => ok(
                        200,
                        json!({"uuid":"{old}","full_name":"corbet-web/corbet.ch"}),
                    ),
                    ("GET", _) => ok(200, World::refs(&self.old_refs)),
                    ("DELETE", "") => {
                        self.old_alive = false;
                        ok(204, Value::Null)
                    }
                    _ => panic!("{method} {path}"),
                };
            }
            let path = path.replace("/corbet-archv/%7Bnew%7D", "/corbet-archv/corbet.ch");
            let path = path.as_str();
            if let Some(rest) = path.strip_prefix(NEW_REPO) {
                return match (method, rest) {
                    ("GET", "") if !self.new_alive => ok(404, Value::Null),
                    ("GET", "") => ok(
                        200,
                        json!({"uuid":"{new}","description":"cfrg-transfer-from:{old}","is_private":true,"scm":"git","project":{"key":"CORE"},"full_name":"corbet-archv/corbet.ch","mainbranch":{"name":"main"}}),
                    ),
                    ("POST", "") => {
                        self.new_alive = true;
                        ok(200, json!({"uuid":"{new}"}))
                    }
                    ("GET", r) if r.starts_with("/refs") => ok(200, World::refs(&self.new_refs)),
                    (_, r) if r.starts_with("/pipelines_config") => {
                        ok(200, json!({"enabled": false}))
                    }
                    ("GET", r) if r.starts_with("/branch-restrictions") => {
                        ok(200, json!({"values": self.restrictions.clone()}))
                    }
                    ("POST", r) if r.starts_with("/branch-restrictions") => {
                        let mut body = r_body(&self.restrictions, &req_body);
                        body["id"] = json!(self.restrictions.len() + 1);
                        self.restrictions.push(body.clone());
                        ok(201, body)
                    }
                    // Restrictions, keys: the protection of the new repository; any shape that says "in place".
                    ("GET", _) => ok(200, json!({"values": []})),
                    _ => ok(200, json!({})),
                };
            }
            panic!("unscripted request {method} {path}")
        }
        fn note(&mut self, kind: &str, key: &str, value: &str) -> Result<()> {
            self.notes
                .insert(format!("{kind}/{key}"), value.to_string());
            Ok(())
        }
        fn recorded(&self, kind: &str, key: &str) -> Option<String> {
            self.notes.get(&format!("{kind}/{key}")).cloned()
        }
        fn credential(&self, _env: &str, _path: &str) -> Option<String> {
            Some("secret".into())
        }
    }

    fn corbet_ch() -> (Repository, Destination) {
        let json = r#"{"provider":"bitbucket","endpoint":{"origin":"https://api.bitbucket.org","token_env":"T"},"path":"corbet-web/corbet.ch","namespace":"CORE","repository_id":"{old}","mirror_user":"9","password_env":"T","interval_seconds":3600,"remote_name":"remote_mirror_0"}"#;
        let mut repo: Repository = serde_json::from_str(&format!(
            r#"{{"path":"corbet-archv/corbet.ch","source_id":5,"private":true,"default_branch":"main","content":"native-git","destinations":[{json}]}}"#
        ))
        .unwrap();
        repo.destinations[0].path = "corbet-archv/corbet.ch".into();
        let dest = repo.destinations[0].clone();
        (repo, dest)
    }

    #[test]
    fn the_whole_bitbucket_move_pending_to_done() {
        let (repo, dest) = corbet_ch();
        let source = cfrg::native::Endpoint {
            origin: "https://forge.example".into(),
            token_env: "F".into(),
        };
        let api_repo = repo.clone();
        let api = cfgj::native::Mirrors {
            endpoint: &source,
            repository: &api_repo,
        };
        let old_mirror = json!({"remote_name":"remote_mirror_0","remote_address":"https://bitbucket.org/corbet-web/corbet.ch.git"});
        let mut world = World {
            mirrors: vec![old_mirror],
            counter: 0,
            old_alive: true,
            new_alive: false,
            old_refs: vec![("main".into(), "a1".into())],
            new_refs: vec![],
            writes: vec![],
            notes: BTreeMap::new(),
            restrictions: vec![],
        };
        let mut mirrors = world.mirrors.clone();
        let mut report = Vec::new();
        let bb = dest.clone();
        let pass = |world: &mut World,
                    mirrors: &mut Vec<Value>,
                    write: bool,
                    dest: &Destination,
                    report: &mut Vec<Value>| {
            let uuid = dest.repository_id.clone().unwrap();
            bitbucket_move(
                world,
                &api,
                &repo,
                dest,
                &bb,
                &uuid,
                "corbet-web",
                mirrors,
                write,
                true,
                report,
            )
            .unwrap()
        };
        // plan: nothing is written, the steps are shown.
        assert!(matches!(
            pass(&mut world, &mut mirrors, false, &dest, &mut report),
            MoveStep::Stop
        ));
        assert_eq!(report.last().unwrap()["state"], "transfer-planned");
        assert!(world.writes.is_empty());
        // apply 1: the new repository is created and a SECOND mirror pushes to it; the old mirror stays.
        assert!(matches!(
            pass(&mut world, &mut mirrors, true, &dest, &mut report),
            MoveStep::Stop
        ));
        assert_eq!(report.last().unwrap()["state"], "transfer-pending");
        assert_eq!(report.last().unwrap()["new_repository_id"], "{new}");
        assert!(world.new_alive && world.old_alive);
        let addresses: Vec<_> = world
            .mirrors
            .iter()
            .map(|m| m["remote_address"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            addresses,
            [
                "https://bitbucket.org/corbet-web/corbet.ch.git",
                "https://bitbucket.org/corbet-archv/corbet.ch.git"
            ]
        );
        // apply 2: the mirror has not pushed yet: still pending, the old repository stays, no second mirror is added.
        assert!(matches!(
            pass(&mut world, &mut mirrors, true, &dest, &mut report),
            MoveStep::Stop
        ));
        assert!(report.last().unwrap()["detail"]
            .as_str()
            .unwrap()
            .contains("do not match"));
        assert!(world.old_alive && world.mirrors.len() == 2);
        // The mirror pushes everything.
        world.new_refs = world.old_refs.clone();
        // apply 3: exact match: the old repository and the old mirror go, the pass continues under the new uuid.
        let step = pass(&mut world, &mut mirrors, true, &dest, &mut report);
        assert!(matches!(&step, MoveStep::Continue(id) if id == "{new}"));
        assert_eq!(report.last().unwrap()["state"], "transferred");
        assert!(!world.old_alive);
        assert_eq!(world.mirrors.len(), 1);
        assert!(world.mirrors[0]["remote_address"]
            .as_str()
            .unwrap()
            .contains("corbet-archv"));
        // The placement still holds the old uuid: the next pass resumes instead of failing.
        let step = pass(&mut world, &mut mirrors, true, &dest, &mut report);
        assert!(matches!(&step, MoveStep::Continue(id) if id == "{new}"));
    }

    #[test]
    fn a_rename_operation_never_creates_the_bitbucket_target() {
        let (repo, dest) = corbet_ch();
        let source = cfrg::native::Endpoint {
            origin: "https://forge.example".into(),
            token_env: "F".into(),
        };
        let api = cfgj::native::Mirrors {
            endpoint: &source,
            repository: &repo,
        };
        let mut world = World {
            mirrors: vec![],
            counter: 0,
            old_alive: true,
            new_alive: false,
            old_refs: vec![("main".into(), "a1".into())],
            new_refs: vec![],
            writes: vec![],
            notes: BTreeMap::new(),
            restrictions: vec![],
        };
        let mut report = Vec::new();
        let step = bitbucket_move(
            &mut world,
            &api,
            &repo,
            &dest,
            &dest,
            "{old}",
            "corbet-web",
            &mut vec![],
            true,
            false,
            &mut report,
        )
        .unwrap();
        assert!(matches!(step, MoveStep::Stop));
        assert_eq!(report[0]["state"], "transfer-blocked");
        assert!(world.writes.is_empty());
    }

    #[test]
    fn the_name_audit_calls_a_namespace_change_a_transfer() {
        assert_eq!(plan_move("old/app", "new/app"), Move::Transfer);
        assert_eq!(
            plan_move("old/app", "old/other"),
            Move::Rename { leaf: "other" }
        );
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
    #[test]
    fn a_bitbucket_copy_is_placed_in_the_project_of_its_visibility_in_the_safe_order() {
        struct Seq(Vec<(&'static str, u16)>, Vec<String>);
        impl Transport for Seq {
            fn send(
                &mut self,
                r: cfrg::native::http::Request,
            ) -> Result<cfrg::native::http::Response> {
                self.1.push(format!("{} {}", r.method, r.path));
                let (m, status) = self.0.remove(0);
                assert_eq!(m, r.method);
                Ok(cfrg::native::http::Response {
                    status,
                    body: Value::Null,
                })
            }
        }
        let doc = |private: bool| {
            format!(
                r#"{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"derive":{{"bitbucket_public_namespace":"PUB","bitbucket_private_namespace":"PRV","owners":{{}}}},"repositories":[{{"path":"o/r","source_id":1,"private":{private},"default_branch":"main","content":"native-git","hold":null,"destinations":[{{"provider":"bitbucket","endpoint":{{"origin":"https://api.bitbucket.org","token_env":"B"}},"path":"o/r","namespace":"CORE","repository_id":null,"mirror_user":"{{u}}","password_env":"B","use_ssh":false,"interval_seconds":3600,"branch_filter":"","hold":null,"absent":false,"remote_name":null}}]}}]}}"#
            )
        };
        let place = |private: bool, io: &mut Seq, existing: Option<Value>, write: bool| {
            let placement = Placement::from_document(doc(private).as_bytes()).unwrap();
            let repo = &placement.repositories[0];
            let dest =
                bitbucket_creation_dest(&repo.destinations[0], repo, placement.derive.as_ref());
            bitbucket_place(io, &dest, repo, existing.as_ref(), write).unwrap()
        };
        // already in the right project: no call at all
        let mut io = Seq(vec![], vec![]);
        assert_eq!(
            place(
                false,
                &mut io,
                Some(json!({"project":{"key":"PUB"},"is_private":false})),
                true
            ),
            "in-place"
        );
        assert!(io.1.is_empty());
        // a copy that is still private, whose primary became public: moved to PUB first (project exists)
        let mut io = Seq(vec![("GET", 200), ("PUT", 200)], vec![]);
        assert_eq!(
            place(
                false,
                &mut io,
                Some(json!({"project":{"key":"PRV"},"is_private":true})),
                true
            ),
            "moved"
        );
        assert_eq!(
            io.1,
            [
                "GET /2.0/workspaces/o/projects/PUB",
                "PUT /2.0/repositories/o/r"
            ]
        );
        // the primary became private but the copy is still public: held, no call (the visibility change comes first)
        let mut io = Seq(vec![], vec![]);
        assert_eq!(
            place(
                true,
                &mut io,
                Some(json!({"project":{"key":"PUB"},"is_private":false})),
                true
            ),
            "held-until-private"
        );
        assert!(io.1.is_empty());
        // the copy is private now: moved into PRV
        let mut io = Seq(vec![("GET", 200), ("PUT", 200)], vec![]);
        assert_eq!(
            place(
                true,
                &mut io,
                Some(json!({"project":{"key":"PUB"},"is_private":true})),
                true
            ),
            "moved"
        );
        // a new copy in a workspace without the project: the project is created first, private exactly when the repo is
        let mut io = Seq(vec![("GET", 404), ("POST", 201)], vec![]);
        assert_eq!(place(true, &mut io, None, true), "project-ready");
        assert_eq!(
            io.1,
            [
                "GET /2.0/workspaces/o/projects/PRV",
                "POST /2.0/workspaces/o/projects"
            ]
        );
        // plan only: nothing is written
        let mut io = Seq(vec![], vec![]);
        assert_eq!(
            place(
                false,
                &mut io,
                Some(json!({"project":{"key":"PRV"},"is_private":true})),
                false
            ),
            "move-planned"
        );
        assert!(io.1.is_empty());
    }

    #[test]
    fn a_pointers_only_gitlab_copy_with_lfs_on_gets_it_switched_off_and_one_without_is_left_alone()
    {
        struct Seq(Vec<(&'static str, u16)>, Vec<String>);
        impl Transport for Seq {
            fn send(
                &mut self,
                r: cfrg::native::http::Request,
            ) -> Result<cfrg::native::http::Response> {
                self.1.push(format!(
                    "{} {} {}",
                    r.method,
                    r.path,
                    r.body.map(|b| b.to_string()).unwrap_or_default()
                ));
                let (m, status) = self.0.remove(0);
                assert_eq!(m, r.method);
                Ok(cfrg::native::http::Response {
                    status,
                    body: Value::Null,
                })
            }
        }
        let placement = Placement::from_document(
            br#"{"schema":1,"source":{"origin":"https://forge.example","token_env":"T"},"repositories":[{"path":"o/r","source_id":1,"private":false,"default_branch":"main","content":"lfs","hold":null,"destinations":[{"provider":"gitlab","endpoint":{"origin":"https://gitlab.com","token_env":"G"},"path":"o/r","namespace":"1","repository_id":null,"mirror_user":"1","password_env":"G","use_ssh":true,"interval_seconds":3600,"branch_filter":"","hold":null,"absent":false,"remote_name":null}]}]}"#,
        )
        .unwrap();
        let dest = &placement.repositories[0].destinations[0];
        let mut io = Seq(vec![("PUT", 200)], vec![]);
        gitlab_lfs_off(&mut io, dest, &json!({"id": 7, "lfs_enabled": true})).unwrap();
        assert_eq!(io.1, [r#"PUT /api/v4/projects/7 {"lfs_enabled":false}"#]);
        let mut io = Seq(vec![], vec![]);
        gitlab_lfs_off(&mut io, dest, &json!({"id": 7, "lfs_enabled": false})).unwrap();
        assert!(io.1.is_empty());
        let mut io = Seq(vec![("PUT", 403)], vec![]);
        assert!(gitlab_lfs_off(&mut io, dest, &json!({"id": 7, "lfs_enabled": true})).is_err());
    }

    #[test]
    fn a_public_bitbucket_copy_is_created_in_the_public_project_and_a_private_one_in_the_owners() {
        let document = |private: bool, derive: &str| {
            format!(
                r#"{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"derive":{derive},"repositories":[{{"path":"o/r","source_id":1,"private":{private},"default_branch":"main","content":"native-git","hold":null,"destinations":[{{"provider":"bitbucket","endpoint":{{"origin":"https://api.bitbucket.org","token_env":"B"}},"path":"o/r","namespace":"CORE","repository_id":null,"mirror_user":"{{u}}","password_env":"B","use_ssh":false,"interval_seconds":3600,"branch_filter":"","hold":null,"absent":false,"remote_name":null}}]}}]}}"#
            )
        };
        let with = r#"{"bitbucket_public_namespace":"PUB","bitbucket_private_namespace":"PRV","owners":{}}"#;
        let public_only = r#"{"bitbucket_public_namespace":"PUB","owners":{}}"#;
        for (private, derive, wanted) in [
            (false, with, "PUB"),
            (true, with, "PRV"),
            (true, public_only, "CORE"),
            (false, r#"{"owners":{}}"#, "CORE"),
        ] {
            let placement = Placement::from_document(document(private, derive).as_bytes()).unwrap();
            let repo = &placement.repositories[0];
            let made =
                bitbucket_creation_dest(&repo.destinations[0], repo, placement.derive.as_ref());
            assert_eq!(made.namespace, wanted, "private={private}");
        }
    }

    #[test]
    fn an_operation_on_an_archived_repository_runs_with_the_archive_lifted_and_restores_it_even_on_failure(
    ) {
        struct Seq(Vec<(&'static str, u16)>, Vec<String>);
        impl Transport for Seq {
            fn send(
                &mut self,
                r: cfrg::native::http::Request,
            ) -> Result<cfrg::native::http::Response> {
                self.1.push(format!(
                    "{} {} {}",
                    r.method,
                    r.path,
                    r.body.map(|b| b.to_string()).unwrap_or_default()
                ));
                let (m, status) = self.0.remove(0);
                assert_eq!(m, r.method);
                Ok(cfrg::native::http::Response {
                    status,
                    body: Value::Null,
                })
            }
        }
        let endpoint = cfrg::native::Endpoint {
            origin: "https://forge.example".into(),
            token_env: "T".into(),
        };
        // success: unarchive, the operation, archive again
        let mut io = Seq(vec![("PATCH", 200), ("PATCH", 200)], vec![]);
        let ran = with_archive_lifted(&mut io, &endpoint, "o/r", |_| Ok(7)).unwrap();
        assert_eq!(ran, 7);
        assert!(
            io.1[0].contains(r#"{"archived":false}"#) && io.1[1].contains(r#"{"archived":true}"#)
        );
        // the operation fails: the archive is still put back and the failure is returned
        let mut io = Seq(vec![("PATCH", 200), ("PATCH", 200)], vec![]);
        let failed: Result<()> =
            with_archive_lifted(&mut io, &endpoint, "o/r", |_| Err("boom".into()));
        assert_eq!(failed.unwrap_err().to_string(), "boom");
        assert_eq!(io.1.len(), 2);
        // unarchiving refused: the operation never runs
        let mut io = Seq(vec![("PATCH", 403)], vec![]);
        let mut ran = false;
        let refused = with_archive_lifted(&mut io, &endpoint, "o/r", |_| {
            ran = true;
            Ok(())
        });
        assert!(refused.is_err() && !ran);
    }

    #[test]
    fn a_locked_deletion_on_an_archived_repository_unarchives_deletes_and_archives_again() {
        struct Seq(Vec<(&'static str, u16)>, Vec<String>);
        impl Transport for Seq {
            fn send(
                &mut self,
                r: cfrg::native::http::Request,
            ) -> Result<cfrg::native::http::Response> {
                self.1.push(format!(
                    "{} {} {}",
                    r.method,
                    r.path,
                    r.body.map(|b| b.to_string()).unwrap_or_default()
                ));
                let (m, status) = self.0.remove(0);
                assert_eq!(m, r.method);
                Ok(cfrg::native::http::Response {
                    status,
                    body: Value::Null,
                })
            }
        }
        let placement = Placement::from_document(
            br#"{"schema":1,"source":{"origin":"https://forge.example","token_env":"T"},"repositories":[{"path":"o/r","source_id":1,"private":false,"default_branch":"main","content":"lfs","hold":null,"destinations":[]}]}"#,
        )
        .unwrap();
        let repo = &placement.repositories[0];
        let api = cfgj::native::Mirrors {
            endpoint: &placement.source,
            repository: repo,
        };
        let mut io = Seq(
            vec![
                ("DELETE", 423),
                ("PATCH", 200),
                ("DELETE", 204),
                ("PATCH", 200),
            ],
            vec![],
        );
        assert!(matches!(
            delete_mirror(&mut io, &api, &placement.source, repo, "remote_mirror_x").unwrap(),
            Deleted::Yes
        ));
        assert!(
            io.1[1].ends_with("{\"archived\":false}") && io.1[3].ends_with("{\"archived\":true}")
        );
        // Unarchiving refused: a hold, no further call.
        let mut io = Seq(vec![("DELETE", 423), ("PATCH", 403)], vec![]);
        assert!(matches!(
            delete_mirror(&mut io, &api, &placement.source, repo, "remote_mirror_x").unwrap(),
            Deleted::Held(_)
        ));
        // A different refusal is a hold without touching the archive.
        let mut io = Seq(vec![("DELETE", 404)], vec![]);
        assert!(matches!(
            delete_mirror(&mut io, &api, &placement.source, repo, "remote_mirror_x").unwrap(),
            Deleted::Held(_)
        ));
    }

    #[test]
    fn only_transport_stops_end_the_run() {
        assert!(stops_the_run(
            "Native API HTTP 429; stopped without retry; window/intent recorded"
        ));
        assert!(stops_the_run("Uncertain prior write; read provider state"));
        assert!(!stops_the_run("Native API HTTP 400; no retry"));
        assert!(!stops_the_run("Native API HTTP 423; no retry"));
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

/// GitHub organisations the gate expects `flagged` (GitHub refuses App installations there). They are read from `orgs.json`
/// beside the gate catalogue (`{"orgs":{"<org>":{"expected":"flagged"}}}`), else from an `orgs` object inside the catalogue.
pub(crate) fn flagged_orgs(catalog: Option<&std::path::Path>) -> Vec<String> {
    let Some(catalog) = catalog else {
        return Vec::new();
    };
    let read = |path: &std::path::Path| -> Option<Value> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    };
    let orgs = catalog
        .parent()
        .and_then(|dir| read(&dir.join("orgs.json")))
        .map(|v| v["orgs"].clone())
        .filter(|v| v.is_object())
        .or_else(|| {
            read(catalog)
                .map(|v| v["orgs"].clone())
                .filter(|v| v.is_object())
        });
    orgs.and_then(|o| {
        o.as_object().map(|m| {
            m.iter()
                .filter(|(_, v)| v["expected"] == "flagged")
                .map(|(k, _)| k.clone())
                .collect()
        })
    })
    .unwrap_or_default()
}

/// The report row for a destination that failed with `message`, and whether it counts as a failure: an absent copy is
/// pending, a copy in a flagged GitHub organisation is skipped, anything else is an error.
fn failure_row(message: &str, flagged: bool, source: &str, destination: &str) -> (Value, bool) {
    if message.contains(cghb::relay::REPOSITORY_ABSENT) {
        (
            json!({"source":source, "destination":destination, "state":"pending", "reason":"copy-not-created-yet"}),
            false,
        )
    } else if flagged {
        (
            json!({"source":source, "destination":destination, "state":"skipped", "reason":"flagged-org-no-app-installation"}),
            false,
        )
    } else {
        (
            json!({"source":source, "destination":destination, "state":"error", "error":message}),
            true,
        )
    }
}

/// Whether the owner (first path segment) of `path` is a flagged GitHub organisation.
pub(crate) fn is_flagged_org(catalog: Option<&std::path::Path>, path: &str) -> bool {
    let owner = path.split('/').next().unwrap_or_default();
    flagged_orgs(catalog).iter().any(|o| o == owner)
}

#[cfg(test)]
mod flagged_tests {
    use super::*;

    #[test]
    fn flagged_orgs_come_from_orgs_json_beside_the_catalogue() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("gate-config.json");
        std::fs::write(&catalog, r#"{"repos":{}}"#).unwrap();
        std::fs::write(
            dir.path().join("orgs.json"),
            r#"{"orgs":{"cmtymt":{"expected":"flagged"},"corbet-libre":{"expected":"normal"}}}"#,
        )
        .unwrap();
        assert_eq!(flagged_orgs(Some(&catalog)), vec!["cmtymt".to_string()]);
        assert!(is_flagged_org(Some(&catalog), "cmtymt/cmeet"));
        assert!(!is_flagged_org(Some(&catalog), "corbet-libre/cfgj"));
        assert!(!is_flagged_org(None, "cmtymt/cmeet"));
    }

    #[test]
    fn a_flagged_org_is_skipped_and_an_unflagged_404_stays_an_error() {
        let (row, failed) = failure_row("Not Found (404)", true, "a/b", "cmtymt/b");
        assert_eq!(row["state"], "skipped");
        assert_eq!(row["reason"], "flagged-org-no-app-installation");
        assert!(!failed);
        let (row, failed) = failure_row("Not Found (404)", false, "a/b", "corbet-libre/b");
        assert_eq!(row["state"], "error");
        assert!(failed);
    }
}
