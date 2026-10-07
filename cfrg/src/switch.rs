//! Primary switch: move a repository's write side from one forge to another
//! without ever running two mirror directions, and without losing a commit.
//!
//! The placement says which forge is the primary; this procedure makes the
//! forges agree with it. It is forge-neutral: every forge rule sits behind
//! [`Replica`], and the edge mechanism comes from [`crate::replicate::choose`].
//!
//! Order, each step idempotent so a failed run is simply run again:
//!
//! * (a) **hash check**: the new primary must hold every branch and tag of the
//!   old one at the same commit; no receiver may hold a ref the new primary
//!   lacks (a mirror would delete it). Anything else refuses, nothing changes.
//! * (c, early) **freeze**: the old primary is locked against every writer, so
//!   nothing can slip in between the check and the change. The hash check runs
//!   once more on the frozen repository; if replication has not caught up the
//!   old primary is unlocked again and the switch refuses.
//! * (b) **disable** the old direction: every owned mirror whose sender is not
//!   the new primary is removed. Mirrors cfrg does not own are never touched; a
//!   foreign mirror between two sites refuses the switch.
//! * (d) **unlock** the new primary for normal landing.
//! * (e) **create** the new direction: for each other site, the mechanism from
//!   [`crate::replicate::choose`]; a native push mirror is created and its
//!   principal (mirror key or credential owner) is the only writer the receiver
//!   admits, which also protects the old primary as a receiver.
//! * (f) **verify**: force the mirrors, then every site must show the new
//!   primary's heads, and only the new primary may own a mirror.
//!
//! Without `apply` nothing is written: the report lists the steps and the
//! checks that read-only calls can answer.
use crate::{
    failure,
    land::Support,
    native::http::Transport,
    replicate::{choose, Admit, Ledger, Method, Mirror, Refs, Replica},
    Result,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    thread,
    time::{Duration, Instant},
};

pub struct Options {
    pub apply: bool,
    /// How long replication may take to catch up (before the freeze and for the
    /// final verification).
    pub drain: Duration,
    pub poll: Duration,
}

/// Refs of `old` that `new` does not hold at the same commit.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Gap {
    pub missing: Vec<String>,
    pub differs: Vec<String>,
}

impl Gap {
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.differs.is_empty()
    }
    fn json(&self) -> Value {
        json!({"missing": self.missing, "differs": self.differs})
    }
}

pub fn gap(old: &Refs, new: &Refs) -> Gap {
    let mut gap = Gap::default();
    for (name, id) in old {
        match new.get(name) {
            None => gap.missing.push(name.clone()),
            Some(other) if other != id => gap.differs.push(name.clone()),
            Some(_) => {}
        }
    }
    gap
}

/// Refs a receiver holds that the new primary lacks: the next mirror run would
/// delete them.
pub fn extras(receiver: &Refs, new: &Refs) -> Vec<String> {
    receiver
        .keys()
        .filter(|name| !new.contains_key(*name))
        .cloned()
        .collect()
}

struct Log {
    apply: bool,
    steps: Vec<Value>,
}

impl Log {
    fn run(
        &mut self,
        step: &str,
        site: &str,
        action: impl Into<String>,
        work: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let action = action.into();
        if self.apply {
            work()?;
        }
        self.steps.push(json!({
            "step": step, "site": site, "action": action,
            "state": if self.apply { "done" } else { "planned" },
        }));
        Ok(())
    }
}

struct Found {
    sender: usize,
    target: usize,
    mirror: Mirror,
    key: String,
}

fn describe(site: &dyn Replica) -> String {
    format!("{}:{}", site.site().forge.as_str(), site.site().path)
}

fn mirrors_between<T: Transport + Ledger>(
    io: &mut T,
    sites: &[&dyn Replica],
    refused: &mut Vec<String>,
) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    for (sender, site) in sites.iter().enumerate() {
        for mirror in site.mirrors(io)? {
            let Some(target) = sites
                .iter()
                .position(|other| other.site().address == mirror.address)
            else {
                continue; // not a site of this repository: not ours to judge
            };
            if target == sender {
                continue;
            }
            let key = site.site().mirror_key(sites[target].site());
            if io.owned(&key).as_deref() != Some(mirror.id.as_str()) {
                refused.push(format!(
                    "unowned mirror {} -> {}: record or declare its remote_name before switching",
                    describe(*site),
                    describe(sites[target])
                ));
            }
            found.push(Found {
                sender,
                target,
                mirror,
                key,
            });
        }
    }
    Ok(found)
}

fn poll<F: FnMut() -> Result<bool>>(options: &Options, mut done: F) -> Result<bool> {
    let deadline = Instant::now() + options.drain;
    loop {
        if done()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(options.poll);
    }
}

/// Switch `repository` from `sites[from]` to `sites[to]`.
pub fn switch<T: Transport + Ledger>(
    io: &mut T,
    repository: &str,
    sites: &[&dyn Replica],
    from: usize,
    to: usize,
    options: &Options,
) -> Result<Value> {
    if from == to || from >= sites.len() || to >= sites.len() {
        return Err(failure("The placement does not change the primary"));
    }
    let (old, new) = (sites[from], sites[to]);
    let mut log = Log {
        apply: options.apply,
        steps: Vec::new(),
    };
    let mut refused: Vec<String> = Vec::new();
    for site in sites {
        if site.capabilities().switch.support == Support::Unsupported {
            refused.push(format!(
                "{} cannot take part in a primary switch",
                describe(*site)
            ));
        }
        site.verify(io)?;
    }

    // Edges and mechanisms, before anything is touched.
    let edges: Vec<(usize, Method)> = (0..sites.len())
        .filter(|index| *index != to)
        .map(|index| {
            (
                index,
                choose(&new.capabilities(), &sites[index].capabilities(), true),
            )
        })
        .collect();
    for (index, method) in &edges {
        if *method == Method::DestinationPullMirror {
            refused.push(format!(
                "{}: destination pull mirror is not implemented; use cfrg sync",
                describe(sites[*index])
            ));
        }
    }

    // (a) The new primary holds everything the old one does.
    let all_refs = |io: &mut T| -> Result<Vec<Refs>> { sites.iter().map(|s| s.refs(io)).collect() };
    let mut refs = all_refs(io)?;
    let mut preflight = gap(&refs[from], &refs[to]);
    for (index, site) in sites.iter().enumerate() {
        if index != to {
            for name in extras(&refs[index], &refs[to]) {
                refused.push(format!(
                    "{} holds {name}, which the new primary lacks: a mirror would delete it",
                    describe(*site)
                ));
            }
        }
    }
    let found = mirrors_between(io, sites, &mut refused)?;

    if options.apply && !refused.is_empty() {
        return Err(failure(format!("Switch refused: {}", refused.join("; "))));
    }
    if !preflight.is_empty() && options.apply {
        // Replication may simply be behind: nudge the old direction and wait.
        for entry in found.iter().filter(|f| f.sender == from) {
            sites[from].run_mirror(io, &entry.mirror)?;
        }
        let caught_up = poll(options, || {
            refs = all_refs(io)?;
            preflight = gap(&refs[from], &refs[to]);
            Ok(preflight.is_empty())
        })?;
        if !caught_up {
            return Err(failure(format!(
                "Switch refused: {} does not hold every ref of {} ({})",
                describe(new),
                describe(old),
                preflight.json()
            )));
        }
    }

    // (c, early) Freeze the old primary, then check once more.
    log.run(
        "freeze",
        &describe(old),
        "lock against every writer",
        || old.lock(io, &Admit::Nobody),
    )?;
    if options.apply {
        let settled = poll(options, || {
            refs = all_refs(io)?;
            preflight = gap(&refs[from], &refs[to]);
            Ok(preflight.is_empty())
        })?;
        if !settled {
            old.unlock(io)?;
            return Err(failure(format!(
                "Switch refused after the freeze (old primary unlocked again): {}",
                preflight.json()
            )));
        }
    }

    // (b) Disable every owned direction that does not start at the new primary.
    for entry in found.iter().filter(|f| f.sender != to) {
        let sender = sites[entry.sender];
        log.run(
            "disable-mirror",
            &describe(sender),
            format!("remove push mirror to {}", describe(sites[entry.target])),
            || {
                sender.remove_mirror(io, &entry.mirror)?;
                io.forget(&entry.key)
            },
        )?;
    }

    // (d) The new primary takes normal landing back.
    log.run("unlock", &describe(new), "remove the receiver lock", || {
        new.unlock(io)
    })?;

    // (e) The new direction, one mechanism per edge.
    let mut forced: Vec<(usize, Mirror)> = Vec::new();
    let mut synced: Vec<String> = Vec::new();
    for (index, method) in &edges {
        let receiver = sites[*index];
        let target = receiver.target()?;
        match method {
            Method::SourcePushMirror => {
                let existing = found
                    .iter()
                    .find(|f| f.sender == to && f.target == *index && f.mirror.enabled)
                    .map(|f| f.mirror.clone());
                let mut created: Option<Mirror> = existing.clone();
                log.run(
                    "create-mirror",
                    &describe(new),
                    format!(
                        "{} push mirror to {}",
                        if existing.is_some() { "keep" } else { "create" },
                        describe(receiver)
                    ),
                    || {
                        if created.is_none() {
                            let mirror = new.add_mirror(io, &target)?;
                            io.record(&new.site().mirror_key(receiver.site()), &mirror.id)?;
                            created = Some(mirror);
                        }
                        Ok(())
                    },
                )?;
                let admit = match created.as_ref().and_then(|m| m.public_key.clone()) {
                    Some(public_key) => Admit::Key {
                        title: format!("cfrg:{}", created.as_ref().map_or("", |m| &m.id)),
                        public_key,
                    },
                    None => Admit::User(target.principal.clone()),
                };
                log.run(
                    "lock",
                    &describe(receiver),
                    match &admit {
                        Admit::Key { .. } => "write only for the mirror key".to_string(),
                        _ => format!("write only for {}", target.principal),
                    },
                    || receiver.lock(io, &admit),
                )?;
                if let Some(mirror) = created {
                    forced.push((*index, mirror));
                }
            }
            Method::Sync => {
                log.run(
                    "lock",
                    &describe(receiver),
                    format!("write only for {}", target.principal),
                    || receiver.lock(io, &Admit::User(target.principal.clone())),
                )?;
                synced.push(describe(receiver));
                log.steps.push(json!({
                    "step": "sync-edge", "site": describe(receiver),
                    "action": "no native mirror: replicate with cfrg sync", "state": "declared",
                }));
            }
            Method::DestinationPullMirror => {}
        }
    }

    // (f) Every site shows the new primary's heads; only the new primary sends.
    let mut heads = json!({});
    let mut verified = true;
    if options.apply {
        for (_, mirror) in &forced {
            new.run_mirror(io, mirror)?;
        }
        let mirrored: BTreeSet<usize> = forced.iter().map(|(i, _)| *i).collect();
        let converged = poll(options, || {
            refs = all_refs(io)?;
            Ok(mirrored.iter().all(|i| refs[*i] == refs[to]))
        })?;
        verified = converged;
        for (index, site) in sites.iter().enumerate() {
            heads[describe(*site)] = json!({
                "refs": refs[index].len(),
                "matches_primary": refs[index] == refs[to],
                "mirrored": mirrored.contains(&index),
            });
        }
        let after = {
            let mut ignored = Vec::new();
            mirrors_between(io, sites, &mut ignored)?
        };
        if after.iter().any(|f| f.sender != to) {
            verified = false;
            refused.push("a mirror outside the new primary still exists".into());
        }
        io.record(&format!("primary:{repository}"), &new.site().web)?;
    }
    Ok(json!({
        "repository": repository,
        "from": describe(old),
        "to": describe(new),
        "apply": options.apply,
        "checks": {"hold": preflight.json(), "refused": refused},
        "edges": edges.iter().map(|(i, m)| json!({"to": describe(sites[*i]), "method": m})).collect::<Vec<_>>(),
        "steps": log.steps,
        "heads": heads,
        "sync_edges": synced,
        "complete": verified && refused.is_empty() && preflight.is_empty(),
    }))
}

#[cfg(test)]
mod tests;
