//! Zero CI on mirror forges (operating model R6, R7, R9, R35, R42): no copy's declared state can enable CI.
//!
//! A destination is the declaration of a copy on GitHub, GitLab or Bitbucket. It used to carry a `ci` switch that
//! made the reconcile leave the provider's CI as it was. That switch is gone, and these tests keep it gone: a
//! destination that names `ci` (true or false) is refused when the placement is loaded, so a stale declaration
//! cannot revive a copy's CI, and the reconcile of every provider switches CI off unconditionally (the provider
//! crates test that against canned API answers).
use cfrg::native::Placement;
use serde_json::{json, Value};

const PROVIDERS: [(&str, &str); 3] = [
    ("github", "https://api.github.com"),
    ("gitlab", "https://gitlab.com"),
    ("bitbucket", "https://api.bitbucket.org"),
];

fn destination(provider: &str, origin: &str) -> Value {
    json!({
        "provider": provider,
        "endpoint": {"origin": origin, "token_env": "TOKEN"},
        "path": "team/repo",
        "namespace": "team",
        "repository_id": "7",
        "mirror_user": "bot",
        "password_env": "TOKEN",
        "interval_seconds": 3600
    })
}

fn document(destinations: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema": 1,
        "source": {"origin": "https://forge.example", "token_env": "SOURCE_TOKEN"},
        "repositories": [{
            "path": "team/repo", "source_id": 1, "private": false, "default_branch": "main",
            "content": "native-git", "destinations": destinations
        }]
    }))
    .unwrap()
}

#[test]
fn a_destination_of_every_provider_loads_and_carries_no_ci_declaration() {
    let all: Vec<Value> = PROVIDERS
        .iter()
        .map(|(provider, origin)| destination(provider, origin))
        .collect();
    let placement = Placement::from_document(&document(all)).unwrap();
    let written = serde_json::to_value(&placement).unwrap();
    for destination in written["repositories"][0]["destinations"]
        .as_array()
        .unwrap()
    {
        assert!(
            destination.get("ci").is_none(),
            "a declared destination has no ci field: {destination}"
        );
    }
}

#[test]
fn no_destination_of_any_provider_can_declare_ci_on_or_off() {
    for (provider, origin) in PROVIDERS {
        for value in [json!(true), json!(false), json!("backup")] {
            let mut declared = destination(provider, origin);
            declared["ci"] = value.clone();
            let error = Placement::from_document(&document(vec![declared]))
                .expect_err("a destination that names ci must be refused")
                .to_string();
            assert!(
                error.contains("ci"),
                "{provider} ci={value}: the refusal names the field: {error}"
            );
        }
    }
}

#[test]
fn one_declaration_of_ci_among_many_destinations_refuses_the_whole_placement() {
    // Fail closed: a single stale declaration stops the load. It does not become a quiet exception.
    let mut with_ci = destination("github", "https://api.github.com");
    with_ci["ci"] = json!(true);
    let mut destinations: Vec<Value> = PROVIDERS
        .iter()
        .map(|(provider, origin)| destination(provider, origin))
        .collect();
    destinations.push(with_ci);
    assert!(Placement::from_document(&document(destinations)).is_err());
}
