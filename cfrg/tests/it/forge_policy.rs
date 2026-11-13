use cfrg::placement::{execution_decision, Decision, Policy, RunState};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn value() -> Value {
    serde_json::from_str(include_str!("../../examples/repository-policy.json")).unwrap()
}
fn policy() -> Policy {
    serde_json::from_value(value()).unwrap()
}

#[test]
fn primary_ci_determines_forge_and_clones_cover_all_declared_providers() {
    let plan = policy().plan("widget").unwrap();
    assert_eq!(plan.primary_forge, "hub");
    assert_eq!(plan.clone_urls.len(), 4);
    assert_eq!(
        plan.clone_urls[2],
        "https://lab.example.org/group/subgroup/widget"
    );
    let mut changed = value();
    changed["repositories"]["widget"]["ci"] = json!("backup");
    changed["repositories"]["widget"]["clone_fallbacks"] = json!(["hub"]);
    let changed: Policy = serde_json::from_value(changed).unwrap();
    assert_eq!(changed.plan("widget").unwrap().primary_forge, "mirror");
}

#[test]
fn unsafe_placements_are_rejected() {
    let mutations: Vec<(&str, Value)> = vec![
        ("/ci/primary/forge", json!("mirror")),
        ("/ci/primary/capabilities", json!([])),
        ("/forges/hub/url", json!("https://token@hub.example.org")),
        (
            "/repositories/widget/locations/hub",
            json!("team/../widget"),
        ),
        ("/repositories/widget/clone_fallbacks", json!(["hub"])),
        ("/placement_rules/0/require", json!(["unknown"])),
        ("/placement_rules/0/forbid", json!(["mirror"])),
    ];
    for (pointer, mutation) in mutations {
        let mut p = value();
        *p.pointer_mut(pointer).unwrap() = mutation;
        assert!(
            serde_json::from_value::<Policy>(p)
                .unwrap()
                .validate()
                .is_err(),
            "{pointer}"
        );
    }
    let mut p = value();
    p["repositories"]["other"] = p["repositories"]["widget"].clone();
    assert!(serde_json::from_value::<Policy>(p)
        .unwrap()
        .validate()
        .is_err());
    let mut p = value();
    p["repositories"]["widget"]["promotion"] = json!("automatic");
    assert!(serde_json::from_value::<Policy>(p).is_err());
}

#[test]
fn the_decision_is_made_for_the_primary_alone() {
    let p = policy();
    let caps = vec!["linux-x86_64".to_string()];
    let decide = |state: Option<RunState>, caps: &[String]| {
        let states: BTreeMap<String, RunState> = state
            .into_iter()
            .map(|s| ("primary".to_string(), s))
            .collect();
        execution_decision(&p, "widget", caps, &states).unwrap()
    };
    let primary = || "primary".to_string();
    assert_eq!(
        decide(Some(RunState::Absent), &caps),
        Decision::Dispatch { ci: primary() }
    );
    assert_eq!(
        decide(Some(RunState::Active), &caps),
        Decision::Attach { ci: primary() }
    );
    assert_eq!(
        decide(Some(RunState::Succeeded), &caps),
        Decision::Reuse { ci: primary() }
    );
    assert_eq!(
        decide(Some(RunState::Failed), &caps),
        Decision::Failed { ci: primary() }
    );
    // Nothing else to turn to: an unavailable or cancelled primary, an unknown one and a missing observation block.
    for state in [
        Some(RunState::Unavailable),
        Some(RunState::CancelledBeforeStart),
        Some(RunState::Unknown),
        None,
    ] {
        assert!(matches!(decide(state, &caps), Decision::Blocked { .. }));
    }
    // Coverage the primary does not declare is never dispatched or reused.
    let native = vec!["darwin-aarch64".to_string()];
    assert!(matches!(
        decide(Some(RunState::Absent), &native),
        Decision::Blocked { .. }
    ));
    assert!(matches!(
        decide(Some(RunState::Succeeded), &native),
        Decision::Blocked { .. }
    ));
    // An observation of a provider the repository does not have, and a request without coverage, are refused.
    let other = BTreeMap::from([("backup".to_string(), RunState::Absent)]);
    assert!(execution_decision(&p, "widget", &caps, &other).is_err());
    assert!(execution_decision(&p, "widget", &[], &BTreeMap::new()).is_err());
}

#[test]
fn a_ci_executor_on_a_mirror_forge_cannot_be_modelled() {
    // No CI runs on a mirror (operating model R6, R7): the placement has no place for an executor there.
    // A fallback executor, however it is named:
    for fallbacks in [json!(["backup"]), json!(["primary"]), json!(["absent"])] {
        let mut p = value();
        p["repositories"]["widget"]["execution_fallbacks"] = fallbacks;
        let message = serde_json::from_value::<Policy>(p)
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("mirror forge") && message.contains("cannot be modelled"),
            "{message}"
        );
    }
    // A provider-hosted or paid executor does not parse at all.
    for execution in ["free-hosted", "paid"] {
        let mut p = value();
        p["ci"]["primary"]["execution"] = json!(execution);
        assert!(serde_json::from_value::<Policy>(p).is_err(), "{execution}");
    }
    // The own CI of a mirror forge is refused, wherever it is declared.
    for driver in ["github-actions", "gitlab-ci", "bitbucket-pipelines"] {
        for ci in ["primary", "backup"] {
            let mut p = value();
            p["ci"][ci]["driver"] = json!(driver);
            let message = serde_json::from_value::<Policy>(p)
                .unwrap()
                .validate()
                .unwrap_err()
                .to_string();
            assert!(
                message.contains("mirror forge"),
                "{driver} on {ci}: {message}"
            );
        }
    }
    // Existing documents carry `execution_fallbacks: []` and keep working; the plan names no executor but the primary.
    let mut p = value();
    p["repositories"]["widget"]["execution_fallbacks"] = json!([]);
    let policy: Policy = serde_json::from_value(p).unwrap();
    policy.validate().unwrap();
    let plan = serde_json::to_value(policy.plan("widget").unwrap()).unwrap();
    assert!(plan.get("execution_fallbacks").is_none());
    assert_eq!(plan["primary_ci"], "primary");
    // And `free_only` stays accepted, with no effect.
    let mut p = value();
    p["free_only"] = json!(false);
    serde_json::from_value::<Policy>(p)
        .unwrap()
        .validate()
        .unwrap();
}
