//! `cfrg native --operation settings`: bring the settings of every copy in line with its
//! Forgejo primary (operating model R42). Without `--apply` it reads only and prints the plan.
//!
//! The copies are found from the primaries, not from the placement: every repository the
//! source lists should have a copy on every forge (R2), at the primary's path unless the
//! placement pins another. A copy that does not exist is reported, never created here.
//! GitHub writes honour the write gate's kill switch and per-organisation pause (an apply
//! that reaches GitHub refuses to start without both), and one run writes to at most
//! `--max-writes` copies, so a sweep never becomes a burst. A contributed project (the
//! placement's `contributed`, R10/R11) is neither read nor written, on the source or a copy.
use super::Options;
use cfrg::{
    contributed::Contributed,
    native::{
        github_copy_owned,
        http::Transport,
        links::{self, PublicGithub},
        owned_by,
        settings::{self, Field, Observed, Plan, Target},
        Endpoint, Provider,
    },
    Result,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const PROVIDERS: [(Provider, &str, &str, &str); 3] = [
    (
        Provider::Github,
        "github",
        "https://api.github.com",
        "CFRG_GITHUB_TOKEN",
    ),
    (
        Provider::Gitlab,
        "gitlab",
        "https://gitlab.com",
        "CFRG_GITLAB_TOKEN",
    ),
    (
        Provider::Bitbucket,
        "bitbucket",
        "https://api.bitbucket.org",
        "CFRG_BITBUCKET_TOKEN",
    ),
];

fn observe(io: &mut dyn Transport, target: &Target) -> Result<Option<Observed>> {
    match target.provider {
        Provider::Github => cghb::settings::observe(io, target),
        Provider::Gitlab => cglb::settings::observe(io, target),
        Provider::Bitbucket => cbkt::settings::observe(io, target),
    }
}

fn head(io: &mut dyn Transport, target: &Target, branch: &str) -> Result<Option<String>> {
    match target.provider {
        Provider::Github => cghb::settings::head(io, target, branch),
        Provider::Gitlab => cglb::settings::head(io, target, branch),
        Provider::Bitbucket => cbkt::settings::head(io, target, branch),
    }
}

/// Counts for one provider and organisation.
#[derive(Default)]
struct Count {
    copies: u32,
    absent: u32,
    converged: u32,
    to_change: u32,
    changed: u32,
    held: u32,
    errors: u32,
    not_owned: u32,
    by_field: BTreeMap<String, u32>,
    held_by: BTreeMap<String, u32>,
    unsupported: BTreeMap<String, u32>,
}

impl Count {
    fn to_json(&self) -> Value {
        json!({"copies":self.copies, "absent":self.absent, "converged":self.converged,
            "to_change":self.to_change, "changed":self.changed, "held":self.held, "errors":self.errors, "not_owned":self.not_owned,
            "changes_by_field":self.by_field, "held_by_reason":self.held_by, "unsupported":self.unsupported})
    }
}

fn field_name(field: Field) -> String {
    serde_json::to_value(field)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

pub fn run(options: &Options) -> Result<()> {
    let creds = super::creds::Creds::new(options.credentials_dir.clone());
    let placement = creds.placement(&std::fs::read(&options.placement)?)?;
    placement.validate()?;
    let contributed = Contributed::from_placement(&placement, &[])?;
    let write = options.apply;
    // The repositories the GitHub gate manages (keys of its catalogue); none without the option.
    let gate_repos: Vec<String> = match &options.github_gate_catalog {
        Some(path) => {
            let catalog: Value = serde_json::from_slice(&std::fs::read(path)?)?;
            catalog["repos"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default()
        }
        None => Vec::new(),
    };
    // Fail closed: a GitHub write needs the write gate's kill switch and pause directory.
    if write
        && options
            .provider
            .as_deref()
            .is_none_or(|only| only == "github")
        && (options.github_kill_switch.is_none() || options.github_paused_dir.is_none())
    {
        return Err(
            "settings --apply on GitHub needs --github-kill-switch and --github-paused-dir (the write gate's files)"
                .into(),
        );
    }
    let mut io = creds.open(&options.state, write)?;
    let mut rows = Vec::new();
    let mut counts: BTreeMap<(String, String), Count> = BTreeMap::new();
    let mut incomplete = false;
    let mut bitbucket_logged = false;

    let primaries = cfgj::native::list_primaries(&mut io, &placement.source)?;
    let mut stopped: BTreeMap<&'static str, String> = BTreeMap::new();
    let mut written = 0usize;
    let budget = options.max_writes.unwrap_or(20);

    for primary in &primaries {
        if options
            .repository
            .as_ref()
            .is_some_and(|only| !only.eq_ignore_ascii_case(&primary.path))
        {
            continue;
        }
        if primary.contributed
            || contributed
                .classify(&placement.source.origin, &primary.path)
                .is_some()
        {
            rows.push(json!({"source":primary.path, "state":"contributed-project", "note":"R10, R11: somebody else's project has no copies of ours"}));
            continue;
        }
        let declared = placement
            .repositories
            .iter()
            .find(|r| r.source_id == primary.id);
        // What a Bitbucket copy's description may link to: the GitHub copy of this primary
        // when anyone can see it (GitHub is read before Bitbucket), and whether the primary
        // has wiki pages at all (read once, only when a link needs it).
        let mut github_public: Option<(String, bool)> = None;
        let mut wiki_pages: Option<bool> = None;
        // The push mirrors Forgejo holds for this primary, read once and only for an apply: a
        // copy is written only when one of them is ours (`owned_by`, R56) and points at it.
        let mut mirrors_cache: Option<Vec<Value>> = None;
        for (provider, name, origin, token_env) in PROVIDERS {
            if options.provider.as_deref().is_some_and(|only| only != name) {
                continue;
            }
            if provider == Provider::Bitbucket && !creds.available(token_env) {
                if !bitbucket_logged {
                    eprintln!("Bitbucket: skipped: no Bitbucket credential mounted");
                    bitbucket_logged = true;
                }
                continue;
            }
            let dest = declared.and_then(|r| {
                r.destinations
                    .iter()
                    .find(|d| d.provider == provider)
                    .map(|d| (r, d))
            });
            let (path, endpoint, hold) = match dest {
                Some((repo, d)) => {
                    let mut live = repo.clone();
                    live.path = primary.path.clone();
                    (
                        live.destination_path(d),
                        d.endpoint.clone(),
                        d.hold.clone().or(repo.hold.clone()),
                    )
                }
                // No destination declared on this forge: the copy sits at the primary's path,
                // and a hold on the repository holds it too.
                None => (
                    primary.path.clone(),
                    Endpoint {
                        origin: origin.into(),
                        token_env: token_env.into(),
                    },
                    declared.and_then(|r| r.hold.clone()),
                ),
            };
            if contributed.classify(&endpoint.origin, &path).is_some() {
                rows.push(json!({"source":primary.path, "provider":name, "destination":path, "state":"contributed-project", "note":"R10, R11: this location is somebody else's project"}));
                continue;
            }
            let org = path.split('/').next().unwrap_or_default().to_string();
            let count = counts.entry((name.into(), org.clone())).or_default();
            if let Some(why) = stopped.get(name) {
                rows.push(json!({"source":primary.path, "provider":name, "destination":path, "state":"provider-stopped", "reason":why}));
                incomplete = true;
                continue;
            }
            let target = Target {
                provider,
                endpoint,
                path: path.clone(),
            };
            // A failed read of our own primary (Forgejo) fails this row only: it is no refusal
            // by the copy's forge, so that forge keeps going.
            let mut on_primary = false;
            let outcome = (|| -> Result<Value> {
                let Some(seen) = observe(&mut io, &target)? else {
                    count.absent += 1;
                    return Ok(
                        json!({"source":primary.path, "provider":name, "destination":path, "state":"absent"}),
                    );
                };
                count.copies += 1;
                let wiki_repo = provider != Provider::Github
                    || seen.private != Some(false)
                    || github_wiki_repo_exists(&path);
                if provider == Provider::Github
                    && seen.private == Some(false)
                    && !options
                        .github_paused_dir
                        .as_ref()
                        .is_some_and(|dir| dir.join(&org).exists())
                {
                    github_public = Some((path.clone(), seen.issues == Some(true)));
                }
                // ONE writer of descriptions: the wanted text, links included, comes from
                // `links::copy_description`; the planner only compares it.
                if provider != Provider::Gitlab && primary.has_wiki && wiki_pages.is_none() {
                    wiki_pages = Some(
                        cfgj::native::wiki_exists(&mut io, &placement.source, &primary.path)
                            .inspect_err(|_| on_primary = true)?,
                    );
                }
                let copy_wiki =
                    provider == Provider::Github && seen.private == Some(false) && wiki_repo;
                let facts = links::Facts {
                    provider,
                    private: primary.settings.private,
                    forge_origin: &placement.source.origin,
                    source_path: &primary.path,
                    issues: primary.issues,
                    wiki: wiki_pages.unwrap_or(false),
                    github: github_public.as_ref().map(|(path, issues)| PublicGithub {
                        path,
                        issues: *issues,
                        wiki: false,
                    }),
                    copy_wiki,
                };
                let mut wanted = primary.settings.clone();
                wanted.description = links::copy_description(&primary.settings.description, &facts);
                let mut plan = settings::plan_with_wiki_repo(provider, &wanted, &seen, wiki_repo);
                // A copy is archived only once it carries everything the primary has.
                if plan.changes.iter().any(|c| c.field == Field::Archive) {
                    let want = cfgj::native::branch_head(
                        &mut io,
                        &placement.source,
                        &primary.path,
                        &primary.settings.default_branch,
                    )
                    .inspect_err(|_| on_primary = true)?;
                    let has = head(&mut io, &target, &primary.settings.default_branch)?;
                    if want.is_none() || want != has {
                        plan.changes.retain(|c| c.field != Field::Archive);
                        plan.held.push(settings::Held {
                            field: Field::Archive,
                            reason: "copy-not-yet-at-the-primary-head",
                            wanted: json!(true),
                        });
                    }
                }
                for held in &plan.held {
                    *count.held_by.entry(held.reason.into()).or_default() += 1;
                }
                for field in &plan.unsupported {
                    *count.unsupported.entry(field_name(*field)).or_default() += 1;
                }
                for change in &plan.changes {
                    *count.by_field.entry(field_name(change.field)).or_default() += 1;
                }
                if !plan.held.is_empty() {
                    count.held += 1;
                }
                if plan.changes.is_empty() {
                    if plan.held.is_empty() {
                        count.converged += 1;
                    }
                    return Ok(row(&primary.path, name, &path, "converged", &plan, None));
                }
                count.to_change += 1;
                let gate_owned = provider == Provider::Github
                    && github_copy_owned(&primary.path, dest.map(|(_, d)| d), &gate_repos);
                if !write {
                    let mut planned = row(&primary.path, name, &path, "planned", &plan, None);
                    if provider == Provider::Github {
                        planned["owned"] = json!(gate_owned);
                        planned["paused"] = json!(options
                            .github_paused_dir
                            .as_ref()
                            .is_some_and(|dir| dir.join(&org).exists()));
                    }
                    return Ok(planned);
                }
                if mirrors_cache.is_none() && provider != Provider::Github {
                    mirrors_cache = Some(match declared {
                        Some(repo) => {
                            let mut live = repo.clone();
                            live.path = primary.path.clone();
                            cfgj::native::Mirrors {
                                endpoint: &placement.source,
                                repository: &live,
                            }
                            .list(&mut io)
                            .inspect_err(|_| on_primary = true)?
                        }
                        None => Vec::new(),
                    });
                }
                let owned = gate_owned
                    || (provider != Provider::Github
                        && declared.is_some_and(|repo| {
                            let mut live = repo.clone();
                            live.path = primary.path.clone();
                            let wanted = format!(
                                "{}/{}",
                                copy_host(provider, &target.endpoint.origin),
                                path
                            );
                            mirrors_cache.iter().flatten().any(|m| {
                                owned_by(&live, placement.copy.as_ref(), m)
                                    && m["remote_address"]
                                        .as_str()
                                        .and_then(cfrg::replicate::address_of)
                                        .is_some_and(|a| a.eq_ignore_ascii_case(&wanted))
                            })
                        }));
                if !owned {
                    count.not_owned += 1;
                    return Ok(row(&primary.path, name, &path, "not-owned", &plan, None));
                }
                if let Some(why) =
                    refused(options, provider, &org, hold.as_deref(), written, budget)
                {
                    return Ok(row(&primary.path, name, &path, why, &plan, None));
                }
                let applied = match provider {
                    Provider::Github => {
                        cghb::settings::apply(&mut io, &target, &primary.settings, &plan.changes)?
                    }
                    Provider::Gitlab => {
                        cglb::settings::apply(&mut io, &target, &primary.settings, &plan.changes)?
                    }
                    Provider::Bitbucket => {
                        cbkt::settings::apply(&mut io, &target, &primary.settings, &plan.changes)?
                    }
                };
                written += 1;
                count.changed += 1;
                Ok(row(
                    &primary.path,
                    name,
                    &path,
                    "applied",
                    &plan,
                    Some(serde_json::to_value(applied)?),
                ))
            })();
            match outcome {
                Ok(value) => {
                    let state = value["state"].as_str().unwrap_or_default();
                    incomplete |= matches!(
                        state,
                        "planned"
                            | "absent"
                            | "write-budget-spent"
                            | "gate-paused"
                            | "gate-kill-switch"
                            | "held-by-declaration"
                    ) || value["held"].as_array().is_some_and(|h| !h.is_empty());
                    rows.push(value);
                }
                Err(error) => {
                    let message = error.to_string();
                    counts.entry((name.into(), org)).or_default().errors += 1;
                    incomplete = true;
                    // A refusal by the forge (auth, rate, plan, server) stops that forge for
                    // the rest of the run: no retry, the window is recorded in the state.
                    // (`expect` failures, like a 422 on one repository, name the status too but
                    // are no refusal: only the transport's "stopped without retry" is.)
                    if !on_primary
                        && (message.contains("stopped without retry")
                            || message.contains("hold for"))
                    {
                        stopped.insert(name, message.clone());
                    }
                    let side = if on_primary { "primary" } else { "copy" };
                    rows.push(json!({"source":primary.path, "provider":name, "destination":path, "state":"error", "on":side, "error":message}));
                }
            }
        }
    }

    let mut summary = serde_json::Map::new();
    let mut totals: BTreeMap<String, Count> = BTreeMap::new();
    for ((provider, org), count) in &counts {
        summary
            .entry(provider.clone())
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .map(|orgs| orgs.insert(org.clone(), count.to_json()));
        let total = totals.entry(provider.clone()).or_default();
        total.copies += count.copies;
        total.absent += count.absent;
        total.converged += count.converged;
        total.to_change += count.to_change;
        total.changed += count.changed;
        total.held += count.held;
        total.errors += count.errors;
        for (k, v) in &count.by_field {
            *total.by_field.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &count.held_by {
            *total.held_by.entry(k.clone()).or_default() += v;
        }
        for (k, v) in &count.unsupported {
            *total.unsupported.entry(k.clone()).or_default() += v;
        }
    }
    let totals: serde_json::Map<String, Value> = totals
        .iter()
        .map(|(provider, count)| (provider.clone(), count.to_json()))
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "complete": !incomplete, "applied": write,
            "totals": totals, "by_organisation": summary, "copies": rows,
        }))?
    );
    if incomplete && write {
        return Err("Some copies were not brought in line; see the report".into());
    }
    Ok(())
}

fn row(
    source: &str,
    provider: &str,
    destination: &str,
    state: &str,
    plan: &Plan,
    applied: Option<Value>,
) -> Value {
    let mut row =
        json!({"source":source, "provider":provider, "destination":destination, "state":state});
    if !plan.converged() || !plan.unsupported.is_empty() {
        row["plan"] = plan.to_json();
    }
    if let Some(applied) = applied {
        row["applied"] = applied;
    }
    row
}

/// Whether `<repo>.wiki.git` of a public GitHub copy can be read (anonymous `git ls-remote`, no
/// credential; GitHub creates the repository with the first wiki page). Any failure counts as
/// "not there", so the copy is treated like a private one until a later run sees it.
fn github_wiki_repo_exists(path: &str) -> bool {
    std::process::Command::new("git")
        .args([
            "ls-remote",
            "--exit-code",
            "--",
            &format!("https://github.com/{path}.wiki.git"),
            "HEAD",
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The host a copy's address on this forge starts with (as `address_of` prints it).
fn copy_host(provider: Provider, origin: &str) -> String {
    match provider {
        Provider::Github => "github.com".into(),
        Provider::Bitbucket => "bitbucket.org".into(),
        Provider::Gitlab => origin
            .split_once("://")
            .map_or(origin, |(_, rest)| rest)
            .trim_end_matches('/')
            .to_ascii_lowercase(),
    }
}

/// Why a write must wait, if it must. GitHub honours the write gate's files.
fn refused(
    options: &Options,
    provider: Provider,
    org: &str,
    hold: Option<&str>,
    written: usize,
    budget: usize,
) -> Option<&'static str> {
    if hold.is_some() {
        return Some("held-by-declaration");
    }
    if provider == Provider::Github {
        if options
            .github_kill_switch
            .as_ref()
            .is_some_and(|p| p.exists())
        {
            return Some("gate-kill-switch");
        }
        if options
            .github_paused_dir
            .as_ref()
            .is_some_and(|dir| dir.join(org).exists())
        {
            return Some("gate-paused");
        }
    }
    (written >= budget).then_some("write-budget-spent")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn options(dir: &std::path::Path, provider: Option<&str>) -> Options {
        let placement = dir.join("placement.json");
        std::fs::write(
            &placement,
            r#"{"schema":1,"source":{"origin":"https://forge.example","token_env":"T"},"repositories":[]}"#,
        )
        .unwrap();
        Options {
            placement,
            state: dir.join("state.json"),
            credentials_dir: None,
            repository: None,
            provider: provider.map(String::from),
            existing_only: false,
            operation: super::super::Operation::Settings,
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
            wiki_max_pushes: 20,
            wiki_push_gap_secs: 2,
        }
    }

    #[test]
    fn an_apply_that_reaches_github_needs_the_write_gate_files_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        for provider in [None, Some("github")] {
            let mut o = options(dir.path(), provider);
            let error = run(&o).unwrap_err().to_string();
            assert!(error.contains("--github-kill-switch"), "{error}");
            o.github_kill_switch = Some(PathBuf::from("/nonexistent/kill"));
            assert!(run(&o)
                .unwrap_err()
                .to_string()
                .contains("--github-paused-dir"));
        }
        // Nothing was opened: no state, no lock.
        assert!(!dir.path().join("state.json").exists());
        assert!(!dir.path().join("state.lock").exists());
    }
}
