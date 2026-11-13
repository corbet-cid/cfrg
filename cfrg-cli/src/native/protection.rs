//! `cfrg native --operation protection`: is the default branch of every copy held for the
//! mirroring alone (R43)? Read-only, one request per copy, for the copies a `settings` run
//! found (`--copies-from REPORT`). Writing the lock is the job of `--operation reconcile`.
use super::Options;
use cfrg::{
    contributed::Contributed,
    native::{
        http::{Http, Transport},
        settings::{Protection, Target},
        Endpoint, Placement, Provider,
    },
    Result,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn defaults(name: &str) -> Option<(Provider, &'static str, &'static str)> {
    match name {
        "github" => Some((
            Provider::Github,
            "https://api.github.com",
            "CFRG_GITHUB_TOKEN",
        )),
        "gitlab" => Some((Provider::Gitlab, "https://gitlab.com", "CFRG_GITLAB_TOKEN")),
        "bitbucket" => Some((
            Provider::Bitbucket,
            "https://api.bitbucket.org",
            "CFRG_BITBUCKET_TOKEN",
        )),
        _ => None,
    }
}

fn observe(
    io: &mut dyn Transport,
    target: &Target,
    private: bool,
    branch: &str,
) -> Result<Protection> {
    match target.provider {
        Provider::Github => cghb::settings::protection(io, target, private, branch),
        Provider::Gitlab => cglb::settings::protection(io, target, branch),
        Provider::Bitbucket => cbkt::settings::protection(io, target),
    }
}

pub fn run(options: &Options) -> Result<()> {
    if options.apply {
        return Err("Protection is observed here; reconcile writes the lock".into());
    }
    let file = options
        .copies_from
        .as_ref()
        .ok_or("--copies-from REPORT (a settings run) is required")?;
    let report: Value = serde_json::from_slice(&std::fs::read(file)?)?;
    let placement = Placement::from_document(&std::fs::read(&options.placement)?)?;
    placement.validate()?;
    let contributed = Contributed::from_placement(&placement, &[])?;
    let mut io = Http::open(&options.state, false)?;
    let primaries: BTreeMap<String, cfgj::native::Primary> =
        cfgj::native::list_primaries(&mut io, &placement.source)?
            .into_iter()
            .map(|p| (p.path.clone(), p))
            .collect();

    let mut rows = Vec::new();
    let mut counts: BTreeMap<(String, String), BTreeMap<&'static str, u32>> = BTreeMap::new();
    let mut stopped: BTreeMap<String, String> = BTreeMap::new();
    for row in report["copies"].as_array().into_iter().flatten() {
        if matches!(
            row["state"].as_str(),
            Some("absent" | "error" | "contributed-project" | "provider-stopped") | None
        ) {
            continue;
        }
        let (Some(name), Some(path), Some(source)) = (
            row["provider"].as_str(),
            row["destination"].as_str(),
            row["source"].as_str(),
        ) else {
            continue;
        };
        if options.provider.as_deref().is_some_and(|only| only != name) {
            continue;
        }
        let (Some((provider, origin, token_env)), Some(primary)) =
            (defaults(name), primaries.get(source))
        else {
            continue;
        };
        // A contributed project (R10, R11) is not asked about, on the source or the copy.
        if primary.contributed
            || contributed
                .classify(&placement.source.origin, source)
                .is_some()
            || contributed.classify(origin, path).is_some()
        {
            continue;
        }
        let org = path.split('/').next().unwrap_or_default().to_string();
        let tally = counts.entry((name.to_string(), org)).or_default();
        if let Some(why) = stopped.get(name) {
            rows.push(json!({"provider":name, "destination":path, "state":"provider-stopped", "reason":why}));
            continue;
        }
        let target = Target {
            provider,
            endpoint: Endpoint {
                origin: origin.into(),
                token_env: token_env.into(),
            },
            path: path.to_string(),
        };
        match observe(
            &mut io,
            &target,
            primary.settings.private,
            &primary.settings.default_branch,
        ) {
            Ok(protection) => {
                let key = match &protection {
                    Protection::Locked => "locked",
                    Protection::Restricted { .. } => "restricted",
                    Protection::Missing => "missing",
                    Protection::Unavailable { .. } => "unavailable",
                };
                *tally.entry(key).or_default() += 1;
                if protection != Protection::Locked {
                    rows.push(
                        json!({"provider":name, "destination":path, "protection":protection}),
                    );
                }
            }
            Err(error) => {
                let message = error.to_string();
                *tally.entry("errors").or_default() += 1;
                if message.contains("stopped without retry") || message.contains("hold for") {
                    stopped.insert(name.to_string(), message.clone());
                }
                rows.push(
                    json!({"provider":name, "destination":path, "state":"error", "error":message}),
                );
            }
        }
    }
    let mut totals: BTreeMap<&str, BTreeMap<&str, u32>> = BTreeMap::new();
    let mut by_org: BTreeMap<&str, BTreeMap<&str, &BTreeMap<&str, u32>>> = BTreeMap::new();
    for ((provider, org), tally) in &counts {
        by_org.entry(provider).or_default().insert(org, tally);
        let total = totals.entry(provider).or_default();
        for (k, v) in tally {
            *total.entry(k).or_default() += v;
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "rule": "R43: the default branch of every copy is protected so that only the mirroring can push",
            "totals": totals, "by_organisation": by_org, "not_locked": rows,
        }))?
    );
    Ok(())
}
