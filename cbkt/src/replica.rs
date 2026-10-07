//! Bitbucket Cloud as one repository of a replica set: reads and push
//! restrictions as a receiver. Free has no push-mirror feature, so as a sender
//! Bitbucket is `cfrg sync` (adapter-only); this adapter declares that.
use crate::native::{call, restrict, rules};
use cfrg::{
    land::{Capability, Support},
    model::Forge,
    native::http::{expect, Transport},
    native::{Destination, Repository},
    replicate::{Admit, Capabilities, Refs, Replica, Site, Target},
    Result,
};

pub const CAPABILITIES: Capabilities = Capabilities {
    forge: Forge::Bitbucket,
    push_mirror: Capability {
        support: Support::AdapterOnly,
        note: "no native push mirror on Bitbucket Cloud: as a sender it is cfrg sync (fast-forward-only Git copy)",
    },
    mirror_key: false,
    pull_mirror: Capability {
        support: Support::Unsupported,
        note: "unsupported: Bitbucket Cloud has no pull mirror and no import API",
    },
    pull_mirror_converts_existing: false,
    receiver_lock: Capability {
        support: Support::Native,
        note: "native: branch restrictions (push for named users, merge for nobody) are enforced on Free",
    },
    switch: Capability {
        support: Support::Native,
        note: "native: branch restriction create, update and delete",
    },
    rename: Capability {
        support: Support::Native,
        note: "native: updating the repository name changes its slug; the old slug stops answering (no redirect), so cfrg addresses the repository by UUID and re-points the Forgejo push mirror",
    },
};

pub struct Bitbucket {
    site: Site,
    repo: Repository,
    dest: Destination,
}

impl Bitbucket {
    pub fn new(repo: &Repository, dest: &Destination) -> Self {
        Self {
            site: Site::new(
                Forge::Bitbucket,
                &dest.endpoint.origin,
                "https://bitbucket.org",
                "bitbucket.org",
                &dest.path,
                dest.repository_id.clone(),
            ),
            repo: repo.clone(),
            dest: dest.clone(),
        }
    }
}

impl Replica for Bitbucket {
    fn site(&self) -> &Site {
        &self.site
    }
    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }
    fn verify(&self, io: &mut dyn Transport) -> Result<()> {
        crate::native::ensure_destination_repo(io, &self.repo, &self.dest, false, false)?
            .map(|_| ())
            .ok_or_else(|| "Bitbucket destination does not exist".into())
    }
    fn refs(&self, io: &mut dyn Transport) -> Result<Refs> {
        let mut refs = Refs::new();
        for page in 1..=50 {
            let value = expect(
                call(
                    io,
                    &self.dest,
                    "GET",
                    &format!("/refs?pagelen=100&page={page}"),
                    None,
                    false,
                )?,
                &[200],
            )?;
            for entry in value["values"]
                .as_array()
                .ok_or("Malformed Bitbucket refs")?
            {
                let name = entry["name"].as_str().ok_or("Missing ref name")?;
                let id = entry["target"]["hash"]
                    .as_str()
                    .ok_or("Missing ref commit")?;
                let prefix = match entry["type"].as_str() {
                    Some("branch") => "refs/heads/",
                    Some("tag") => "refs/tags/",
                    _ => continue,
                };
                refs.insert(format!("{prefix}{name}"), id.into());
            }
            if value["next"].is_null() {
                return Ok(refs);
            }
            // Never follow a returned URL: only our own bounded page counter.
        }
        Err("Bitbucket refs pagination exceeded".into())
    }
    fn target(&self) -> Result<Target> {
        Ok(Target {
            site: self.site.clone(),
            https: format!("https://bitbucket.org/{}.git", self.dest.path),
            key_url: None,
            login: "x-bitbucket-api-token-auth".into(),
            secret_env: self.dest.password_env.clone(),
            principal: self.dest.mirror_user.clone(),
        })
    }
    fn lock(&self, io: &mut dyn Transport, admit: &Admit) -> Result<()> {
        match admit {
            Admit::Nobody => restrict(io, &self.dest, true, &[]).map(|_| ()),
            Admit::User(uuid) => restrict(io, &self.dest, true, &[uuid.as_str()]).map(|_| ()),
            Admit::Key { .. } => {
                Err("Bitbucket Cloud has no mirror key: restrictions name users".into())
            }
        }
    }
    fn unlock(&self, io: &mut dyn Transport) -> Result<()> {
        for rule in rules(io, &self.dest)? {
            let owned = matches!(rule["kind"].as_str(), Some("push" | "restrict_merges"))
                && rule["pattern"] == "*";
            if !owned {
                continue;
            }
            let id = rule["id"].as_u64().ok_or("Missing restriction ID")?;
            expect(
                call(
                    io,
                    &self.dest,
                    "DELETE",
                    &format!("/branch-restrictions/{id}"),
                    None,
                    false,
                )?,
                &[204],
            )?;
        }
        Ok(())
    }
}
