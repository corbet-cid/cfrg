use super::*;
use crate::{
    land::Capability,
    model::Forge,
    native::http::{Request, Response},
    replicate::{address_of, Capabilities, Site, Target},
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

/// In-memory forges: refs, mirrors, locks. A mirror run copies the sender's
/// refs to the receiver when the receiver admits the mirror's principal.
#[derive(Default)]
struct World {
    refs: BTreeMap<String, Refs>,
    /// (sender, receiver) -> mirror
    mirrors: Vec<(String, String, Mirror)>,
    /// receiver -> who may write
    locks: BTreeMap<String, Admit>,
    log: Vec<String>,
    next: u32,
}

type Shared = Rc<RefCell<World>>;

struct Fake {
    site: Site,
    world: Shared,
    native_sender: bool,
}

fn cap(support: Support) -> Capability {
    Capability { support, note: "" }
}

impl Replica for Fake {
    fn site(&self) -> &Site {
        &self.site
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            forge: self.site.forge,
            push_mirror: cap(if self.native_sender {
                Support::Native
            } else {
                Support::AdapterOnly
            }),
            mirror_key: false,
            pull_mirror: cap(Support::Unsupported),
            pull_mirror_converts_existing: false,
            receiver_lock: cap(Support::Native),
            switch: cap(Support::Native),
            rename: cap(Support::Native),
        }
    }
    fn verify(&self, _io: &mut dyn Transport) -> Result<()> {
        Ok(())
    }
    fn refs(&self, _io: &mut dyn Transport) -> Result<Refs> {
        Ok(self.world.borrow().refs[&self.site.path].clone())
    }
    fn target(&self) -> Result<Target> {
        Ok(Target {
            site: self.site.clone(),
            https: format!("https://{}.git", self.site.address),
            key_url: None,
            login: "mirror".into(),
            secret_env: "SECRET".into(),
            principal: format!("principal-of-{}", self.site.path),
        })
    }
    fn mirrors(&self, _io: &mut dyn Transport) -> Result<Vec<Mirror>> {
        Ok(self
            .world
            .borrow()
            .mirrors
            .iter()
            .filter(|(from, _, _)| *from == self.site.path)
            .map(|(_, _, m)| m.clone())
            .collect())
    }
    fn add_mirror(&self, _io: &mut dyn Transport, to: &Target) -> Result<Mirror> {
        let mut world = self.world.borrow_mut();
        world.next += 1;
        let mirror = Mirror {
            id: format!("m{}", world.next),
            address: address_of(&to.https).unwrap(),
            public_key: None,
            enabled: true,
            healthy: None,
        };
        world
            .log
            .push(format!("add {} -> {}", self.site.path, to.site.path));
        world
            .mirrors
            .push((self.site.path.clone(), to.site.path.clone(), mirror.clone()));
        Ok(mirror)
    }
    fn remove_mirror(&self, _io: &mut dyn Transport, mirror: &Mirror) -> Result<()> {
        let mut world = self.world.borrow_mut();
        world
            .log
            .push(format!("remove {} {}", self.site.path, mirror.id));
        world.mirrors.retain(|(_, _, m)| m.id != mirror.id);
        Ok(())
    }
    fn run_mirror(&self, _io: &mut dyn Transport, mirror: &Mirror) -> Result<()> {
        let mut world = self.world.borrow_mut();
        let to = world
            .mirrors
            .iter()
            .find(|(_, _, m)| m.id == mirror.id)
            .map(|(_, to, _)| to.clone());
        if let Some(to) = to {
            let copy = world.refs[&self.site.path].clone();
            world.refs.insert(to, copy);
        }
        Ok(())
    }
    fn lock(&self, _io: &mut dyn Transport, admit: &Admit) -> Result<()> {
        let mut world = self.world.borrow_mut();
        world.log.push(format!("lock {} {admit:?}", self.site.path));
        world.locks.insert(self.site.path.clone(), admit.clone());
        Ok(())
    }
    fn unlock(&self, _io: &mut dyn Transport) -> Result<()> {
        let mut world = self.world.borrow_mut();
        world.log.push(format!("unlock {}", self.site.path));
        world.locks.remove(&self.site.path);
        Ok(())
    }
}

#[derive(Default)]
struct Mem(BTreeMap<String, String>);
impl Transport for Mem {
    fn send(&mut self, _request: Request) -> Result<Response> {
        Err("the fake forges never use HTTP".into())
    }
}
impl Ledger for Mem {
    fn owned(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
    fn record(&mut self, key: &str, value: &str) -> Result<()> {
        self.0.insert(key.into(), value.into());
        Ok(())
    }
    fn forget(&mut self, key: &str) -> Result<()> {
        self.0.remove(key);
        Ok(())
    }
}

fn refs(pairs: &[(&str, &str)]) -> Refs {
    pairs
        .iter()
        .map(|(a, b)| (format!("refs/heads/{a}"), b.to_string()))
        .collect()
}

struct Setup {
    world: Shared,
    sites: Vec<Fake>,
    ledger: Mem,
}

/// forgejo (primary, native sender) -> gitlab and bitbucket (receivers);
/// gitlab is a native sender too, bitbucket is not.
fn setup() -> Setup {
    let world: Shared = Rc::default();
    let mut sites = Vec::new();
    for (forge, host, native_sender) in [
        (Forge::Forgejo, "forge.example", true),
        (Forge::Gitlab, "gitlab.example", true),
        (Forge::Bitbucket, "bitbucket.example", false),
    ] {
        let path = host.split('.').next().unwrap().to_string();
        world
            .borrow_mut()
            .refs
            .insert(path.clone(), refs(&[("main", "a1")]));
        sites.push(Fake {
            site: Site::new(
                forge,
                &format!("https://{host}"),
                &format!("https://{host}"),
                host,
                &path,
                None,
            ),
            world: world.clone(),
            native_sender,
        });
    }
    let mut ledger = Mem::default();
    for target in 1..3 {
        let from = sites[0].site.clone();
        let to = sites[target].site.clone();
        let mut w = world.borrow_mut();
        w.next += 1;
        let mirror = Mirror {
            id: format!("m{}", w.next),
            address: to.address.clone(),
            public_key: None,
            enabled: true,
            healthy: Some(true),
        };
        ledger.record(&from.mirror_key(&to), &mirror.id).unwrap();
        w.mirrors.push((from.path.clone(), to.path.clone(), mirror));
        w.locks
            .insert(to.path.clone(), Admit::User("principal".into()));
    }
    Setup {
        world,
        sites,
        ledger,
    }
}

fn run(setup: &mut Setup, from: usize, to: usize, apply: bool) -> Result<Value> {
    let sites: Vec<&dyn Replica> = setup.sites.iter().map(|s| s as &dyn Replica).collect();
    switch(
        &mut setup.ledger,
        "team/project",
        &sites,
        from,
        to,
        &Options {
            apply,
            drain: Duration::ZERO,
            poll: Duration::ZERO,
        },
    )
}

#[test]
fn plan_writes_nothing_and_lists_the_order() {
    let mut s = setup();
    let report = run(&mut s, 0, 1, false).unwrap();
    assert!(s.world.borrow().log.is_empty());
    let steps: Vec<_> = report["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["step"].as_str().unwrap())
        .collect();
    assert_eq!(
        steps,
        [
            "freeze",
            "disable-mirror",
            "disable-mirror",
            "unlock",
            "create-mirror",
            "lock",
            "create-mirror",
            "lock"
        ]
    );
    assert_eq!(report["edges"][0]["method"], "source-push-mirror");
    // Bitbucket cannot send, but as a receiver of GitLab it still gets a native mirror.
    assert_eq!(report["edges"][1]["method"], "source-push-mirror");
}

#[test]
fn apply_runs_one_direction_at_a_time_and_ends_with_matching_heads() {
    let mut s = setup();
    let report = run(&mut s, 0, 1, true).unwrap();
    assert_eq!(report["complete"], true, "{report}");
    let world = s.world.borrow();
    // Only the new primary sends, to both others.
    assert!(world.mirrors.iter().all(|(from, _, _)| from == "gitlab"));
    assert_eq!(world.mirrors.len(), 2);
    // The old primary was frozen first, removed mirrors before unlocking the new one.
    let log = world.log.join("|");
    let freeze = log.find("lock forge Nobody").unwrap();
    let removed = log.find("remove forge").unwrap();
    let unlock = log.find("unlock gitlab").unwrap();
    let created = log.find("add gitlab").unwrap();
    assert!(
        freeze < removed && removed < unlock && unlock < created,
        "{log}"
    );
    // The old primary is now a receiver admitting only the mirror principal.
    assert_eq!(
        world.locks["forge"],
        Admit::User("principal-of-forge".into())
    );
    assert!(!world.locks.contains_key("gitlab"));
    assert_eq!(s.ledger.0["primary:team/project"], "https://gitlab.example");
}

#[test]
fn a_new_primary_that_lacks_a_ref_refuses_and_changes_nothing() {
    let mut s = setup();
    s.world
        .borrow_mut()
        .refs
        .insert("gitlab".into(), refs(&[("main", "old")]));
    // The mirror cannot catch up in the fake because the old mirror is not run.
    s.world.borrow_mut().mirrors.clear();
    let error = run(&mut s, 0, 1, true).unwrap_err().to_string();
    assert!(error.contains("does not hold every ref"), "{error}");
    assert!(s.world.borrow().log.is_empty());
    let plan = run(&mut s, 0, 1, false).unwrap();
    assert_eq!(plan["checks"]["hold"]["differs"][0], "refs/heads/main");
    assert_eq!(plan["complete"], false);
}

#[test]
fn a_receiver_holding_an_extra_ref_refuses() {
    let mut s = setup();
    s.world
        .borrow_mut()
        .refs
        .insert("bitbucket".into(), refs(&[("main", "a1"), ("stray", "z")]));
    let error = run(&mut s, 0, 1, true).unwrap_err().to_string();
    assert!(error.contains("would delete it"), "{error}");
}

#[test]
fn a_mirror_cfrg_does_not_own_refuses_instead_of_being_touched() {
    let mut s = setup();
    s.ledger.0.clear();
    let error = run(&mut s, 0, 1, true).unwrap_err().to_string();
    assert!(error.contains("unowned mirror"), "{error}");
    assert_eq!(s.world.borrow().mirrors.len(), 2);
}

#[test]
fn switching_back_restores_the_first_direction() {
    let mut s = setup();
    run(&mut s, 0, 1, true).unwrap();
    let report = run(&mut s, 1, 0, true).unwrap();
    assert_eq!(report["complete"], true, "{report}");
    let world = s.world.borrow();
    assert!(world.mirrors.iter().all(|(from, _, _)| from == "forge"));
    assert_eq!(world.mirrors.len(), 2);
    assert!(!world.locks.contains_key("forge"));
    assert_eq!(s.ledger.0["primary:team/project"], "https://forge.example");
}

#[test]
fn a_sender_without_a_native_mirror_gets_sync_edges_and_a_locked_receiver() {
    let mut s = setup();
    // Bitbucket as the new primary: its edges are cfrg sync.
    s.world.borrow_mut().mirrors.clear();
    s.ledger.0.clear();
    let report = run(&mut s, 0, 2, true).unwrap();
    assert_eq!(report["edges"][0]["method"], "sync");
    assert_eq!(report["sync_edges"].as_array().unwrap().len(), 2);
    assert!(s.world.borrow().mirrors.is_empty());
}

#[test]
fn unsupported_forges_and_unchanged_primaries_refuse() {
    let mut s = setup();
    assert!(run(&mut s, 1, 1, false).is_err());
}
