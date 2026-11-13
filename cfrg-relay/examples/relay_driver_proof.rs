//! Live proof of the full `RouterForge` surface of the GitLab and Forgejo adapters on two throwaway repositories.
//!
//! `relay_driver_proof <forgejo repo id> <forgejo path> <gitlab project id> <gitlab path>`
//! Credentials: the files `forgejo-token` and `gitlab-token` of `$CREDENTIALS_DIRECTORY`. Relay identity ids
//! come from `$PROOF_FORGEJO_RELAY_ID` and `$PROOF_GITLAB_RELAY_ID`. Writes only into the two named repositories.
use cfrg::{
    model::Forge,
    relay::{
        forge::{NewObject, ReadOutcome},
        merge::Patch,
        model::{IssueState, Kind, ObjectRef, RepoRef, RepoSet, RouterConfig},
    },
};
use std::{collections::BTreeMap, env};

fn main() {
    let a: Vec<String> = env::args().collect();
    let (fj_id, fj_path, gl_id, gl_path) = (&a[1], &a[2], &a[3], &a[4]);
    let member = |host: &str, id: &str, path: &str| RepoRef {
        host: host.into(),
        id: id.into(),
        path: path.into(),
    };
    let config = RouterConfig {
        relay: BTreeMap::from([
            (
                Forge::Forgejo,
                vec![env::var("PROOF_FORGEJO_RELAY_ID").unwrap()],
            ),
            (
                Forge::Gitlab,
                vec![env::var("PROOF_GITLAB_RELAY_ID").unwrap()],
            ),
        ]),
        repos: vec![RepoSet {
            name: fj_path.clone(),
            credit: Default::default(),
            members: BTreeMap::from([
                (Forge::Forgejo, member("forge.corbet.ch", fj_id, fj_path)),
                (Forge::Gitlab, member("gitlab.com", gl_id, gl_path)),
            ]),
        }],
        ..Default::default()
    };
    let rt = cfrg_relay::live::runtime().unwrap();
    let dir = cfrg_relay::live::default_credentials_dir();
    let mut forges = cfrg_relay::live::forges(&dir, &rt, &config, "https://forge.corbet.ch");
    for (forge, repo) in [(Forge::Forgejo, fj_id), (Forge::Gitlab, gl_id)] {
        let f = forges.get(forge).expect("adapter");
        let marker = format!("<!-- cforge:proof-{} -->", forge.as_str());
        let created = f
            .create_object(
                repo,
                &NewObject {
                    title: "B3b throwaway proof".into(),
                    body: format!("proof body\n{marker}"),
                    labels: vec!["proof".into()],
                },
            )
            .unwrap();
        println!("{forge:?} created #{}", created.number);
        let o = ObjectRef {
            forge,
            repo: repo.clone(),
            kind: Kind::Issue,
            number: created.number,
        };
        let ReadOutcome::Found(state) = f.read(&o).unwrap() else {
            panic!("created object not found")
        };
        println!(
            "{forge:?} read: author {} labels {:?}",
            state.author.id, state.snap.labels
        );
        let c = f.create_comment(&o, "first comment").unwrap();
        f.update_comment(&o, c, "first comment (edited)").unwrap();
        f.update_object(
            &o,
            &Patch {
                title: Some("B3b throwaway proof (edited)".into()),
                body: Some(format!("edited body\n{marker}")),
                labels_add: ["proof2".to_string()].into(),
                labels_remove: ["proof".to_string()].into(),
                ..Default::default()
            },
        )
        .unwrap();
        f.update_object(
            &o,
            &Patch {
                state: Some(IssueState::Closed),
                ..Default::default()
            },
        )
        .unwrap();
        let ReadOutcome::Found(after) = f.read(&o).unwrap() else {
            panic!("gone")
        };
        println!(
            "{forge:?} after: {:?} {:?} labels {:?} comments {:?}",
            after.snap.title,
            after.snap.state,
            after.snap.labels,
            after
                .comments
                .iter()
                .map(|c| c.body.as_str())
                .collect::<Vec<_>>()
        );
        let found = f.find_by_marker(repo, &marker).unwrap();
        println!("{forge:?} find_by_marker: {:?}", found.map(|c| c.number));
        println!(
            "{forge:?} is_maintainer(relay): {:?}",
            f.is_maintainer(repo, &state.author).unwrap()
        );
        f.delete_comment(&o, c).unwrap();
        f.delete_comment(&o, c).unwrap();
        println!("{forge:?} comment deleted twice (second = already gone)");
        match f.delete_object(&o) {
            Ok(()) => println!("{forge:?} object deleted"),
            Err(e) => println!("{forge:?} delete_object: {e}"),
        }
        println!(
            "{forge:?} read after delete: {:?}",
            f.read(&o).map(|r| matches!(r, ReadOutcome::Gone))
        );
    }
}
