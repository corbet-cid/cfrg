//! Live proof of the contribution notice on a throwaway Forgejo repository.
//!
//! `notice_proof <forgejo owner/repo> <origin https url> <ref> <head sha>`; token in `$PROOF_FORGEJO_TOKEN`.
//! Creates the notice, or moves its marker to the new head; prints what it did.
use cfrg::relay::notice::{sync_notice, ContributionMarker};
use cfrg_relay::notices_live::ForgejoNotices;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let token = std::env::var("PROOF_FORGEJO_TOKEN").unwrap();
    let rt = cfrg_relay::live::runtime().unwrap();
    let origin = std::env::var("PROOF_FORGEJO_ORIGIN").unwrap();
    let driver = cfgj::relay::ForgejoRelay::new(&origin, &cfgj::relay::Token::new(token)).unwrap();
    let mut host = ForgejoNotices { driver, rt };
    let marker = ContributionMarker {
        origin: a[2].clone(),
        reference: a[3].clone(),
        sha: a[4].clone(),
    };
    let done = sync_notice(
        &mut host,
        &a[1],
        "Throwaway contribution (B13b proof)",
        "Relayed from a throwaway GitLab project.",
        &marker,
    );
    println!("{done:?}");
}
