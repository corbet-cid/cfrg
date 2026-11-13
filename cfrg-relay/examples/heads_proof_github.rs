//! Live proof of the GitHub pull request heads: a throwaway Forgejo pull request (the origin) shown on a
//! throwaway private GitHub repository through the App installation.
//!
//! `heads_proof_github <sync|take> <github copy> <forgejo owner/repo> <pr number> <head sha>`
//! Environment: `PROOF_APP_ID`, `PROOF_APP_KEY`, `PROOF_INSTALLATION_ID`, `PROOF_FORGEJO_TOKEN`.
use cfrg::{
    model::Forge,
    relay::{
        forge::ForgeError,
        heads::{Copy, GitCli, HeadHost, Heads, Origin, ReqRef, ReqState, Site},
    },
};
use cfrg_relay::heads_live::{GithubHeads, Rest};
use std::{env, rc::Rc};

include!("forgejo_origin.rs.inc");

fn main() {
    let a: Vec<String> = env::args().collect();
    let (mode, copy, fj, n, sha) = (&a[1], &a[2], &a[3], a[4].parse::<u64>().unwrap(), &a[5]);
    let var = |k: &str| env::var(k).unwrap();
    let rt = cfrg_relay::live::runtime().unwrap();
    let creds = cghb::relay::AppCredentials {
        app_id: var("PROOF_APP_ID").parse().unwrap(),
        installation_id: var("PROOF_INSTALLATION_ID").parse().unwrap(),
        private_key_pem: cghb::relay::Redacted::new(var("PROOF_APP_KEY")),
    };
    let driver = cghb::relay::GithubRelay::new(&creds, "https://api.github.com").unwrap();
    let fjt = var("PROOF_FORGEJO_TOKEN");
    let mut heads = Heads::new(Box::new(GitCli));
    heads.insert(Box::new(GithubHeads::new(Rc::clone(&rt), driver).unwrap()));
    heads.insert(Box::new(ForgejoOrigin(
        Rest::new(
            "https://forge.corbet.ch/api/v1",
            &format!("Authorization: token {fjt}"),
        )
        .unwrap(),
    )));
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
        forge: Forge::Github,
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
