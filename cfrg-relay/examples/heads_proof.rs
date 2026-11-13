//! Live proof of the pull request heads on a throwaway GitLab project and a throwaway Forgejo repository.
//!
//! `heads_proof <sync|take> <gitlab copy path> <forgejo owner/repo> <pr number> <head sha>`
//! Credentials: `$CREDENTIALS_DIRECTORY/gitlab-token` (the service account) and `$PROOF_FORGEJO_TOKEN`.
//! The origin is the Forgejo pull request; the GitLab copy shows its head from a relay fork.
use cfrg::{
    model::Forge,
    relay::{
        forge::ForgeError,
        heads::{Copy, GitCli, HeadHost, Heads, Origin, ReqRef, ReqState, Site},
    },
};
use cfrg_relay::heads_live::GitlabHeads;
use std::{env, fs, rc::Rc};

include!("forgejo_origin.rs.inc");

fn main() {
    let a: Vec<String> = env::args().collect();
    let (mode, copy, fj, n, sha) = (&a[1], &a[2], &a[3], a[4].parse::<u64>().unwrap(), &a[5]);
    let dir = env::var("CREDENTIALS_DIRECTORY").unwrap();
    let gl = fs::read_to_string(format!("{dir}/gitlab-token"))
        .unwrap()
        .trim()
        .to_owned();
    let fjt = env::var("PROOF_FORGEJO_TOKEN").unwrap();
    let rt = cfrg_relay::live::runtime().unwrap();
    let driver = rt
        .block_on(cglb::relay::GitlabRelay::new(
            "gitlab.com",
            &cglb::relay::Token::new(gl.clone()),
            false,
        ))
        .unwrap();
    let mut heads = Heads::new(Box::new(GitCli));
    heads.insert(Box::new(
        GitlabHeads::new(Rc::clone(&rt), driver, "gitlab.com", &gl).unwrap(),
    ));
    heads.insert(Box::new(ForgejoOrigin {
        rt: Rc::clone(&rt),
        driver: cfgj::relay::ForgejoRelay::new(
            "https://forge.corbet.ch",
            &cfgj::relay::Token::new(fjt.clone()),
        )
        .unwrap(),
    }));
    let origin = Origin {
        forge: Forge::Forgejo,
        repo: fj.clone(),
        number: n,
        head_sha: sha.clone(),
        fetch_url: format!("https://x:{fjt}@forge.corbet.ch/{fj}.git"),
        title: "B8 throwaway proof".into(),
        body: "Relayed head of a throwaway pull request (lane B8 proof).".into(),
        state: ReqState::Open,
    };
    let copies = [Copy {
        forge: Forge::Gitlab,
        repo: copy.clone(),
        base: "main".into(),
    }];
    let reports = if mode == "take" {
        heads.take_in(&origin, &copies, sha)
    } else {
        heads.sync(&origin, &copies)
    };
    for r in reports {
        println!("{} {} -> {:?}", r.forge.as_str(), r.repo, r.result);
    }
}
