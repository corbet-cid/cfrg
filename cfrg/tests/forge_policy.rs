use cfrg::placement::{execution_decision, Decision, Policy, RunState};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn value() -> Value {
    serde_json::from_str(include_str!("../examples/repository-policy.json")).unwrap()
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
    changed["repositories"]["widget"]["execution_fallbacks"] = json!([]);
    let changed: Policy = serde_json::from_value(changed).unwrap();
    assert_eq!(changed.plan("widget").unwrap().primary_forge, "mirror");
}

#[test]
fn unsafe_placements_and_paid_or_private_hosted_execution_are_rejected() {
    let mutations: Vec<(&str, Value)> = vec![
        ("/ci/primary/execution", json!("paid")),
        ("/ci/primary/forge", json!("mirror")),
        ("/ci/primary/capabilities", json!([])),
        ("/forges/hub/url", json!("https://token@hub.example.org")),
        ("/repositories/widget/visibility", json!("private")),
        ("/repositories/widget/sensitive", json!(true)),
        (
            "/repositories/widget/locations/hub",
            json!("team/../widget"),
        ),
        ("/repositories/widget/clone_fallbacks", json!(["hub"])),
        (
            "/repositories/widget/execution_fallbacks",
            json!(["absent"]),
        ),
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
fn fallback_reconciles_all_providers_and_preserves_failures_and_native_coverage() {
    let p = policy();
    let caps = vec!["linux-x86_64".into()];
    let mut states = BTreeMap::from([
        ("primary".into(), RunState::Unavailable),
        ("backup".into(), RunState::Absent),
    ]);
    assert_eq!(
        execution_decision(&p, "widget", &caps, &states).unwrap(),
        Decision::Dispatch {
            ci: "backup".into()
        }
    );
    states.insert("primary".into(), RunState::Failed);
    assert_eq!(
        execution_decision(&p, "widget", &caps, &states).unwrap(),
        Decision::Failed {
            ci: "primary".into()
        }
    );
    states.insert("primary".into(), RunState::Unknown);
    assert!(matches!(
        execution_decision(&p, "widget", &caps, &states).unwrap(),
        Decision::Blocked { .. }
    ));
    states.insert("primary".into(), RunState::Absent);
    states.insert("backup".into(), RunState::Active);
    assert_eq!(
        execution_decision(&p, "widget", &caps, &states).unwrap(),
        Decision::Attach {
            ci: "backup".into()
        }
    );
    states.insert("backup".into(), RunState::Succeeded);
    assert_eq!(
        execution_decision(&p, "widget", &caps, &states).unwrap(),
        Decision::Reuse {
            ci: "backup".into()
        }
    );
    states.insert("primary".into(), RunState::CancelledBeforeStart);
    states.insert("backup".into(), RunState::Absent);
    assert!(matches!(
        execution_decision(&p, "widget", &["darwin-aarch64".into()], &states).unwrap(),
        Decision::Blocked { .. }
    ));
}
