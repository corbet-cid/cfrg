//! The relay's repository set, derived from the placement (`native.repositories`), never from a hand list (R44).
//!
//! A repository is relayed when it is a Forgejo primary with at least one GitHub or GitLab copy. Bitbucket is a git
//! mirror by design and never takes part; a repository with only Bitbucket copies (or none) is not listed; a
//! contributed project (R10, R11) is not listed, whatever its copies. A copy is addressed by its immutable id, so a
//! copy whose `repository_id` the placement does not know yet is not a member (the relay never guesses an id from
//! a path); the Forgejo primary always is, by its `source_id`.
use super::model::{RepoRef, RepoSet};
use crate::{
    contributed::Contributed,
    model::Forge,
    native::{Placement, Provider},
};
use std::collections::{BTreeMap, BTreeSet};

fn host_of(origin: &str) -> String {
    let rest = origin
        .trim()
        .strip_prefix("https://")
        .unwrap_or(origin.trim());
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    // The API host of GitHub is not the web host the contributed-project check compares.
    host.strip_prefix("api.").map(str::to_owned).unwrap_or(host)
}

/// The repository sets of `placement`, sorted by name.
pub fn repos_from_placement(placement: &Placement, contributed: &Contributed) -> Vec<RepoSet> {
    let source_host = host_of(&placement.source.origin);
    let skip: BTreeSet<String> = contributed.source_paths().into_iter().collect();
    let mut sets = Vec::new();
    for repo in &placement.repositories {
        if skip.contains(&repo.path.to_ascii_lowercase()) {
            continue;
        }
        let copies: Vec<_> = repo
            .destinations
            .iter()
            .filter(|d| matches!(d.provider, Provider::Github | Provider::Gitlab) && !d.absent)
            .collect();
        if copies.is_empty() {
            continue;
        }
        let mut members = BTreeMap::new();
        members.insert(
            Forge::Forgejo,
            RepoRef {
                host: source_host.clone(),
                id: repo.source_id.to_string(),
                path: repo.path.clone(),
            },
        );
        for copy in copies {
            let (forge, id) = match (&copy.provider, &copy.repository_id) {
                (Provider::Github, Some(id)) => (Forge::Github, id),
                (Provider::Gitlab, Some(id)) => (Forge::Gitlab, id),
                _ => continue,
            };
            members.insert(
                forge,
                RepoRef {
                    host: host_of(&copy.endpoint.origin),
                    id: id.clone(),
                    path: copy.path.clone(),
                },
            );
        }
        sets.push(RepoSet {
            name: repo.path.clone(),
            credit: Default::default(),
            members,
        });
    }
    sets.sort_by(|a, b| a.name.cmp(&b.name));
    sets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(provider: &str, id: Option<&str>) -> String {
        let id = id.map_or("null".to_owned(), |i| format!("\"{i}\""));
        format!(
            r#"{{"provider":"{provider}","endpoint":{{"origin":"https://{h}","token_env":"T"}},"path":"o/r","namespace":"n","repository_id":{id},
            "mirror_user":"u","password_env":"T","use_ssh":false,"interval_seconds":3600,"branch_filter":"","hold":null,"absent":false,"remote_name":null}}"#,
            h = if provider == "github" {
                "api.github.com"
            } else {
                "gitlab.com"
            }
        )
    }

    fn repo(path: &str, source_id: u64, dests: &[String]) -> String {
        format!(
            r#"{{"path":"{path}","source_id":{source_id},"private":false,"default_branch":"main","content":"profile","hold":null,"destinations":[{}]}}"#,
            dests.join(",")
        )
    }

    #[test]
    fn only_forgejo_primaries_with_github_or_gitlab_copies_and_no_contributed() {
        let doc = format!(
            r#"{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"F"}},"repositories":[{},{},{},{},{}],
            "contributed":[{{"name":"up","upstream":"https://github.com/x/up","forks":["https://forge.example/o/contrib"]}}]}}"#,
            repo(
                "o/both",
                1,
                &[dest("gitlab", Some("77")), dest("bitbucket", None)]
            ),
            repo("o/bitbucket-only", 2, &[dest("bitbucket", None)]),
            repo("o/none", 3, &[]),
            repo("o/contrib", 4, &[]),
            repo("o/unknown-id", 5, &[dest("github", None)]),
        );
        let placement = Placement::from_document(doc.as_bytes()).unwrap();
        let contributed = Contributed::from_placement(&placement, &[]).unwrap();
        let sets = repos_from_placement(&placement, &contributed);
        let names: Vec<_> = sets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["o/both", "o/unknown-id"]);
        assert_eq!(sets[0].members[&Forge::Forgejo].id, "1");
        assert_eq!(sets[0].members[&Forge::Gitlab].id, "77");
        assert_eq!(sets[0].members.len(), 2, "Bitbucket is never a member");
        assert_eq!(
            sets[1].members.len(),
            1,
            "a copy without an immutable id is not addressed"
        );
    }
}
