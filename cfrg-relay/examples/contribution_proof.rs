//! Live proof of the contribution flow on three throwaway repositories: `contribution_proof <event|takein> <origin forge> <number>`.
//!
//! Environment: `PROOF_CREDENTIALS` (credentials directory as the unit mounts it), `PROOF_SET` (JSON `RepoSet` of the
//! throwaway primary and copies), `PROOF_RELAY` (JSON map forge -> [relay user ids]), `PROOF_FORGEJO_ORIGIN`.
//! State is in memory: every run is one event against the live forges; the forges themselves are the truth (RD10).
use cfrg::{
    contributed::Contributed,
    model::Forge,
    native::Placement,
    relay::{
        model::{Hint, HintAction, Kind, RepoSet, RouterConfig},
        pacing::SystemClock,
        store::MemStore,
        Engine, Outcome, Screen, ScreenVerdict,
    },
};
use std::{env, path::PathBuf};

struct PassAll;
impl Screen for PassAll {
    fn screen(&self, _f: Forge, _o: &cfrg::relay::model::ObjectState) -> ScreenVerdict {
        ScreenVerdict::Pass
    }
}

fn main() {
    let a: Vec<String> = env::args().collect();
    let (mode, origin, number) = (
        &a[1],
        Forge::parse(&a[2]).unwrap(),
        a[3].parse::<u64>().unwrap(),
    );
    let var = |k: &str| env::var(k).unwrap();
    let dir = Some(PathBuf::from(var("PROOF_CREDENTIALS")));
    let set: RepoSet = serde_json::from_str(&var("PROOF_SET")).unwrap();
    let mut cfg = RouterConfig {
        relay: serde_json::from_str(&var("PROOF_RELAY")).unwrap(),
        repos: vec![set.clone()],
        ..RouterConfig::default()
    };
    cfg.pacing.write_gap_secs = 2;
    cfg.pacing.create_gap_secs = 12;
    cfg.validate().unwrap();
    let rt = cfrg_relay::live::runtime().unwrap();
    let forgejo = var("PROOF_FORGEJO_ORIGIN");
    let drivers = cfrg_relay::live::Drivers::open(&dir, &rt, &forgejo);
    let mut forges = drivers.into_forges(&rt, &cfg, &Default::default());
    let mut contrib = cfrg_relay::live_contrib(&dir, &rt, &forgejo);
    let placement = Placement::from_document(
        br#"{"native":{"schema":1,"source":{"origin":"https://forge.example","token_env":"T"},"repositories":[],"contributed":[]}}"#,
    )
    .unwrap();
    let contributed = Contributed::from_placement(&placement, &[]).unwrap();
    let mut store = MemStore::default();
    let clock = SystemClock;
    let mut engine = Engine {
        cfg: &cfg,
        store: &mut store,
        forges: &mut forges,
        clock: &clock,
        screen: &PassAll,
        contributed: &contributed,
        holder: "proof",
        contrib: contrib.as_mut(),
    };
    let hint = Hint {
        forge: origin,
        repo: set.members[&origin].id.clone(),
        kind: Kind::Contribution,
        number,
        action: HintAction::Changed,
        actor: None,
    };
    let report = if mode == "takein" {
        let sha = env::var("PROOF_SHA").unwrap();
        engine.take_in_contribution(&hint, &sha).unwrap()
    } else {
        engine.apply(&hint).unwrap()
    };
    println!("{}", serde_json::to_string(&report).unwrap());
    if !matches!(report.outcome, Outcome::Converged) {
        std::process::exit(2);
    }
}
