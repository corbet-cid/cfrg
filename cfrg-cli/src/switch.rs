use cfrg::{
    native::{
        http::{Http, Pacing, Request, Response, Transport},
        Content, Placement, Provider,
    },
    replicate::{self, Ledger, Replica},
    switch::{self, Options as Run},
    Result,
};
use clap::Args;
use serde_json::json;
use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};

#[derive(Args)]
pub struct Options {
    /// The one declared placement: `lib/placement.json`, or its native view.
    #[arg(long, required_unless_present = "capabilities")]
    placement: Option<PathBuf>,
    /// Ownership of mirrors, rate windows and uncertain writes (never credentials).
    #[arg(long, required_unless_present = "capabilities")]
    state: Option<PathBuf>,
    /// `owner/repo` to switch.
    #[arg(long, required_unless_present = "capabilities")]
    repository: Option<String>,
    /// The placement as it was before the change (for example `git show HEAD~1:lib/placement.json`).
    #[arg(long, conflicts_with = "from")]
    previous: Option<PathBuf>,
    /// Base URL of the current primary, when neither `--previous` nor the recorded state says it.
    #[arg(long)]
    from: Option<String>,
    /// Act. Without it nothing is written.
    #[arg(long)]
    apply: bool,
    /// Seconds replication may take to catch up (before the freeze and at the end).
    #[arg(long, default_value_t = 120)]
    drain: u64,
    /// Print what each forge adapter declares for replication and exit.
    #[arg(long)]
    capabilities: bool,
}

const PACING: Pacing = Pacing {
    read: 1,
    write: 5,
    creation: 60,
};

pub fn capabilities() -> serde_json::Value {
    replicate::table(&[
        cfgj::replica::CAPABILITIES,
        cglb::replica::CAPABILITIES,
        cbkt::replica::CAPABILITIES,
        cghb::REPLICATION,
    ])
}

/// The native request state plus the mirror names the placement declares as owned.
struct Claims<'a> {
    http: &'a mut Http,
    declared: BTreeMap<String, String>,
}

impl Transport for Claims<'_> {
    fn send(&mut self, request: Request) -> Result<Response> {
        self.http.send(request)
    }
    fn resolve(&mut self, scope: &str) -> Result<()> {
        self.http.resolve(scope)
    }
}

impl Ledger for Claims<'_> {
    fn owned(&self, key: &str) -> Option<String> {
        self.http
            .owned(key)
            .or_else(|| self.declared.get(key).cloned())
    }
    fn record(&mut self, key: &str, value: &str) -> Result<()> {
        self.http.record(key, value)
    }
    fn forget(&mut self, key: &str) -> Result<()> {
        self.declared.remove(key);
        self.http.forget(key)
    }
}

pub fn run(options: Options) -> Result<()> {
    if options.capabilities {
        println!("{}", serde_json::to_string_pretty(&capabilities())?);
        return Ok(());
    }
    let (placement_path, state_path, path) =
        match (&options.placement, &options.state, &options.repository) {
            (Some(p), Some(s), Some(r)) => (p, s, r),
            _ => return Err("--placement, --state and --repository are required".into()),
        };
    let placement = Placement::from_document(&fs::read(placement_path)?)?;
    placement.validate()?;
    let repo = placement
        .repositories
        .iter()
        .find(|r| r.path == *path)
        .ok_or("Selected repository is not declared")?;
    if repo.hold.is_some() || repo.content != Content::NativeGit {
        return Err("Repository is held or not plain Git content: no switch".into());
    }
    if repo.destinations.iter().any(|d| d.hold.is_some()) {
        return Err("A destination of this repository is held: lift the hold first".into());
    }

    let mut replicas: Vec<Box<dyn Replica>> = vec![Box::new(cfgj::replica::Forgejo::new(
        &placement.source,
        repo,
        placement.receiver.as_ref(),
    ))];
    for dest in repo.destinations.iter().filter(|d| !d.absent) {
        replicas.push(match dest.provider {
            Provider::Gitlab => Box::new(cglb::replica::Gitlab::new(repo, dest)),
            Provider::Bitbucket => Box::new(cbkt::replica::Bitbucket::new(repo, dest)),
        });
    }
    let sites: Vec<&dyn Replica> = replicas.iter().map(|r| r.as_ref()).collect();
    let index_of = |web: &str| {
        let web = web.trim_end_matches('/');
        sites
            .iter()
            .position(|s| s.site().web == web)
            .ok_or_else(|| format!("No site of {path} is at {web}"))
    };
    let to = index_of(&placement.primary_of(path))?;

    let mut http = Http::open(state_path, options.apply)?.paced(PACING);
    let from_url = if let Some(url) = &options.from {
        url.clone()
    } else if let Some(previous) = &options.previous {
        Placement::from_document(&fs::read(previous)?)?.primary_of(path)
    } else {
        http.owned(&format!("primary:{path}"))
            .unwrap_or_else(|| placement.source.origin.clone())
    };
    let from = index_of(&from_url)?;

    let forgejo = sites[0].site();
    let declared = repo
        .destinations
        .iter()
        .filter(|d| !d.absent)
        .zip(sites.iter().skip(1))
        .filter_map(|(dest, site)| {
            dest.remote_name
                .clone()
                .map(|name| (forgejo.mirror_key(site.site()), name))
        })
        .collect();
    let mut io = Claims {
        http: &mut http,
        declared,
    };
    let report = switch::switch(
        &mut io,
        path,
        &sites,
        from,
        to,
        &Run {
            apply: options.apply,
            drain: Duration::from_secs(options.drain),
            poll: Duration::from_secs(5),
        },
    );
    match report {
        Ok(report) => {
            println!("{}", serde_json::to_string_pretty(&report)?);
            if options.apply && report["complete"] != true {
                return Err("Switch finished incomplete".into());
            }
            Ok(())
        }
        Err(error) => {
            println!(
                "{}",
                json!({"repository": path, "apply": options.apply, "refused": error.to_string()})
            );
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfrg::{
        model::Forge,
        replicate::{choose, Method},
    };

    fn caps(forge: Forge) -> cfrg::replicate::Capabilities {
        let table = [
            cfgj::replica::CAPABILITIES,
            cglb::replica::CAPABILITIES,
            cbkt::replica::CAPABILITIES,
            cghb::REPLICATION,
        ];
        *table.iter().find(|c| c.forge == forge).unwrap()
    }

    #[test]
    fn declared_capabilities_choose_native_mirrors_only_where_a_sender_has_them() {
        let forges = [
            Forge::Forgejo,
            Forge::Gitlab,
            Forge::Bitbucket,
            Forge::Github,
        ];
        for sender in forges {
            for receiver in forges.into_iter().filter(|r| *r != sender) {
                let expected = match sender {
                    Forge::Forgejo | Forge::Gitlab => Method::SourcePushMirror,
                    // No pull mirror converts an existing repository: sync is the fallback.
                    Forge::Bitbucket | Forge::Github => Method::Sync,
                };
                assert_eq!(
                    choose(&caps(sender), &caps(receiver), true),
                    expected,
                    "{sender:?} -> {receiver:?}"
                );
            }
        }
        // A brand new Forgejo repository can be a pull mirror of a sender without push mirrors.
        assert_eq!(
            choose(&caps(Forge::Bitbucket), &caps(Forge::Forgejo), false),
            Method::DestinationPullMirror
        );
    }

    #[test]
    fn the_table_lists_every_adapter() {
        let table = capabilities();
        for forge in ["forgejo", "gitlab", "bitbucket", "github"] {
            assert!(
                table[forge]["push_mirror"]["support"].is_string(),
                "{forge}"
            );
        }
    }
}
