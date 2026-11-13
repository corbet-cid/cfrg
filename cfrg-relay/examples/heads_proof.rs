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
use cfrg_relay::heads_live::{enc, GitlabHeads, Rest};
use std::{env, fs, rc::Rc};

struct ForgejoOrigin(Rest);
impl HeadHost for ForgejoOrigin {
    fn forge(&self) -> Forge {
        Forge::Forgejo
    }
    fn site(&mut self, _: &str) -> Result<Site, ForgeError> {
        Err(ForgeError::Rejected("origin only".into()))
    }
    fn branch_tip(&mut self, _: &Site, _: &str) -> Result<Option<String>, ForgeError> {
        Ok(None)
    }
    fn find_request(&mut self, _: &str, _: &Site, _: &str) -> Result<Option<ReqRef>, ForgeError> {
        Ok(None)
    }
    fn open_request(
        &mut self,
        _: &str,
        _: &Site,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<ReqRef, ForgeError> {
        Err(ForgeError::Rejected("origin only".into()))
    }
    fn request_state(&mut self, repo: &str, n: u64) -> Result<Option<ReqState>, ForgeError> {
        let v = self
            .0
            .call_opt("GET", &format!("repos/{repo}/pulls/{n}"), &[])?;
        Ok(v.map(|v| {
            if v["merged"].as_bool() == Some(true) {
                ReqState::Merged
            } else if v["state"].as_str() == Some("closed") {
                ReqState::Closed
            } else {
                ReqState::Open
            }
        }))
    }
    fn close_request(&mut self, repo: &str, n: u64, note: &str) -> Result<(), ForgeError> {
        self.0.call_opt(
            "POST",
            &format!("repos/{repo}/issues/{n}/comments"),
            &[("body", note)],
        )?;
        self.0.call_opt(
            "PATCH",
            &format!("repos/{repo}/pulls/{n}"),
            &[("state", "closed")],
        )?;
        Ok(())
    }
    fn delete_branch(&mut self, _: &Site, _: &str) -> Result<(), ForgeError> {
        Ok(())
    }
}

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
        forge: Forge::Gitlab,
        repo: copy.clone(),
        base: "main".into(),
    }];
    let _ = enc("");
    let reports = if mode == "take" {
        heads.take_in(&origin, &copies, sha)
    } else {
        heads.sync(&origin, &copies)
    };
    for r in reports {
        println!("{} {} -> {:?}", r.forge.as_str(), r.repo, r.result);
    }
}
