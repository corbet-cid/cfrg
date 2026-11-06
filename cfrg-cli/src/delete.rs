//! `cfrg gate delete-repo`: delete one old private GitHub repository, narrowly.
//!
//! Only repositories named in a declared allowlist can be deleted, each with the commit its default branch must still be
//! at. The verdict order is: allowlist, kill switch / paused organisation / pacing (the gate), then two GitHub reads
//! (private? head?). A public repository, an unlisted one or a moved head is refused. The ledger entry is written before
//! the DELETE call, so a crash can only over-count.
use crate::gate::{api_base, failure, github, now, HttpAnswer};
use cfrg::{
    gate::{self, Config, Entry, Kind, Ledger},
    Result,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};

/// The allowlist: repository name to the commit its default branch must be at.
struct Allowlist {
    repos: BTreeMap<String, String>,
}

fn load(path: &Path) -> Result<Allowlist> {
    let bad = |text: String| failure(format!("Invalid delete allowlist: {text}"));
    let value: Value =
        serde_json::from_str(&std::fs::read_to_string(path)?).map_err(|e| bad(e.to_string()))?;
    let object = value
        .as_object()
        .filter(|o| o.len() == 1)
        .ok_or_else(|| bad("expected exactly the key repos".into()))?;
    let repos = object
        .get("repos")
        .and_then(Value::as_object)
        .ok_or_else(|| bad("repos must be an object".into()))?;
    let mut list = BTreeMap::new();
    for (name, entry) in repos {
        gate::split(name)?;
        let entry = entry
            .as_object()
            .filter(|o| o.len() == 1)
            .ok_or_else(|| bad(format!("{name} must be exactly {{\"head\": ...}}")))?;
        let head = entry
            .get("head")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if head.len() != 40 || !head.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(bad(format!(
                "head of {name} must be a lowercase 40-hex commit"
            )));
        }
        list.insert(name.clone(), head.to_owned());
    }
    Ok(Allowlist { repos: list })
}

fn plain_branch(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(['.', '-'])
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn json_of(answer: &HttpAnswer, what: &str) -> Result<Value> {
    if answer.status != 200 {
        return Err(failure(format!(
            "GitHub API HTTP {} reading {what}",
            answer.status
        )));
    }
    serde_json::from_slice(&answer.body)
        .map_err(|_| failure(format!("Malformed GitHub answer for {what}")))
}

/// The two read-only facts that must hold before a deletion: the repository is private and its default branch is at
/// the listed commit. Returns the default branch name.
pub(crate) fn vet(
    name: &str,
    listed_head: &str,
    repo: &Value,
    branch: impl FnOnce(&str) -> Result<Value>,
) -> Result<String> {
    let full = repo
        .get("full_name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !full.eq_ignore_ascii_case(name) {
        return Err(failure(format!("GitHub answered for {full:?}, not {name}")));
    }
    if repo.get("private").and_then(Value::as_bool) != Some(true) {
        return Err(failure(format!(
            "{name} is not private: refusing to delete a public repository"
        )));
    }
    let default = repo
        .get("default_branch")
        .and_then(Value::as_str)
        .filter(|b| plain_branch(b))
        .ok_or_else(|| failure(format!("{name} has no plain default branch name")))?;
    let head = branch(default)?
        .pointer("/commit/sha")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| failure("GitHub branch answer lacks the commit"))?;
    if head != listed_head {
        return Err(failure(format!(
            "{name} {default} is at {head}, the allowlist expects {listed_head}: refusing"
        )));
    }
    Ok(default.to_owned())
}

pub fn delete_repo(
    config: &Config,
    token_env: &str,
    api: &str,
    allowlist: &Path,
    name: &str,
    execute: bool,
) -> Result<()> {
    let (org, repo_name) = gate::split(name)?;
    let list = load(allowlist)?;
    let Some(listed_head) = list.repos.get(name) else {
        return Err(failure(format!("{name} is not on the delete allowlist")));
    };
    let api = api_base(api)?;
    let now = now();
    let ledger = Ledger::new(&config.state_dir);
    let entries = ledger.load(now)?;
    let world = gate::World {
        now,
        ledger: &entries,
        kill_switch_set: config.kill_switch.exists(),
        org_paused: config.paused_dir.join(org).exists(),
    };
    if let Err(deny) = gate::decide(
        &config.budgets,
        &world,
        gate::Lane::Bulk,
        org,
        repo_name,
        Kind::Delete,
    ) {
        println!("{}", json!({"repo": name, "verdict": deny.text()}));
        return Err(failure(deny.text()));
    }
    // Verification reads GitHub with the credential, in the dry run as well: a dry run that cannot verify says nothing.
    let token = std::env::var(token_env).map_err(|_| {
        failure(format!(
            "{token_env} is not set; nothing can be verified or deleted"
        ))
    })?;
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(failure("Invalid credential format"));
    }
    let repo_path = format!("/repos/{org}/{repo_name}");
    let repo = json_of(
        &github(config, org, &api, &token, "GET", &repo_path, None)?,
        "the repository",
    )?;
    let default = vet(name, listed_head, &repo, |branch| {
        json_of(
            &github(
                config,
                org,
                &api,
                &token,
                "GET",
                &format!("{repo_path}/branches/{branch}"),
                None,
            )?,
            "the default branch",
        )
    })?;
    if !execute {
        println!(
            "{}",
            json!({"repo": name, "verdict": "would delete", "private": true, "default_branch": default, "head": listed_head})
        );
        return Ok(());
    }
    // Count first: a crash between this line and the call can only over-count.
    ledger.append(&Entry {
        ts: now,
        kind: Kind::Delete,
        org: org.into(),
        repo: repo_name.into(),
        lane: gate::Lane::Bulk,
    })?;
    let answer = github(config, org, &api, &token, "DELETE", &repo_path, None)?;
    if answer.status != 204 {
        return Err(failure(format!(
            "GitHub API HTTP {} deleting {name}",
            answer.status
        )));
    }
    println!(
        "{}",
        json!({"repo": name, "verdict": "deleted", "head": listed_head})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn repo(private: bool) -> Value {
        json!({"full_name": "Old/Thing", "private": private, "default_branch": "main"})
    }
    fn at(sha: &str) -> impl FnOnce(&str) -> Result<Value> + '_ {
        move |branch| {
            assert_eq!(branch, "main");
            Ok(json!({"commit": {"sha": sha}}))
        }
    }

    #[test]
    fn a_private_repository_at_the_listed_head_passes() {
        assert_eq!(
            vet("old/thing", HEAD, &repo(true), at(HEAD)).unwrap(),
            "main"
        );
    }

    #[test]
    fn a_public_repository_is_refused_before_any_branch_read() {
        let error = vet("old/thing", HEAD, &repo(false), |_| {
            panic!("no read for a public repo")
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("not private"), "{error}");
    }

    #[test]
    fn a_moved_head_is_refused() {
        let moved = "f".repeat(40);
        let error = vet("old/thing", HEAD, &repo(true), at(&moved))
            .unwrap_err()
            .to_string();
        assert!(error.contains("allowlist expects"), "{error}");
    }

    #[test]
    fn an_answer_for_another_repository_or_a_odd_branch_is_refused() {
        assert!(vet("other/thing", HEAD, &repo(true), at(HEAD)).is_err());
        let odd = json!({"full_name": "old/thing", "private": true, "default_branch": "a/../b"});
        assert!(vet("old/thing", HEAD, &odd, at(HEAD)).is_err());
    }

    #[test]
    fn the_allowlist_is_strict() {
        let dir = tempfile::tempdir().unwrap();
        let write = |text: &str| {
            let path = dir.path().join("list.json");
            std::fs::write(&path, text).unwrap();
            load(&path)
        };
        assert!(write(&format!(r#"{{"repos":{{"o/r":{{"head":"{HEAD}"}}}}}}"#)).is_ok());
        assert!(write(r#"{"repos":{"o/r":{"head":"abc"}}}"#).is_err());
        assert!(write(&format!(
            r#"{{"repos":{{"o/r":{{"head":"{}"}}}}}}"#,
            HEAD.to_uppercase()
        ))
        .is_err());
        assert!(write(&format!(
            r#"{{"repos":{{"o/r":{{"head":"{HEAD}","x":1}}}}}}"#
        ))
        .is_err());
        assert!(write(&format!(
            r#"{{"repos":{{"bad name":{{"head":"{HEAD}"}}}}}}"#
        ))
        .is_err());
    }
}
