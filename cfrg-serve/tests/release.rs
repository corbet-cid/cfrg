//! The Python bridge's release-upload tests, ported case by case (the parity contract of Bridge build B5), plus the
//! contributed-project refusal (R10, R11).
mod common;
use common::*;
use serde_json::json;
use std::collections::BTreeMap;

fn statuses(out: &cfrg_serve::release::Outcome) -> Vec<String> {
    vec![status_of(out, "github"), status_of(out, "gitlab")]
}

#[tokio::test]
async fn fresh_sync_then_idempotent_noop() {
    let w = World::new().await;
    let first = w.sync(false).await;
    assert_eq!(first.exit, 0, "{first:?}");
    assert_eq!(status_of(&first, "github"), "synced");
    assert_eq!(status_of(&first, "gitlab"), "synced");
    assert_eq!(status_of(&first, "bitbucket"), "skipped");
    let files = w.st.lock().unwrap().files.clone();
    assert_eq!(w.gh_stored(), files);
    {
        let st = w.st.lock().unwrap();
        let gitlab: BTreeMap<_, _> = st
            .l_files
            .iter()
            .map(|(n, d)| (n.clone(), d[0].clone()))
            .collect();
        assert_eq!(gitlab, files);
        assert_eq!(st.g_releases[0]["draft"], false);
        assert_eq!(st.g_releases[0]["body"], "Notes\r\nline two");
        let mut names: Vec<String> = st
            .l_links
            .iter()
            .map(|l| l["name"].as_str().unwrap().to_owned())
            .collect();
        names.sort();
        assert_eq!(names, files.keys().cloned().collect::<Vec<_>>());
        assert!(st.l_links.iter().all(|l| l["link_type"] == "package"));
    }
    let before = (w.gh.writes().len(), w.gl.writes().len());
    let again = w.sync(false).await;
    assert_eq!(again.exit, 0);
    assert_eq!(statuses(&again), ["in_sync", "in_sync"]);
    assert_eq!(
        (w.gh.writes().len(), w.gl.writes().len()),
        before,
        "an idempotent re-run must write nothing"
    );
}

#[tokio::test]
async fn credential_does_not_follow_a_redirect_to_the_object_store() {
    let w = World::new().await;
    w.sync(false).await;
    let seen = w.store.all();
    assert!(
        seen.iter().any(|r| r.path.starts_with("/blob/")),
        "the SHA256SUMS read-back must follow the redirect"
    );
    for r in seen {
        assert!(r.header("authorization").is_none());
    }
}

#[tokio::test]
async fn plan_writes_nothing() {
    let w = World::new().await;
    let out = w.sync(true).await;
    assert_eq!(statuses(&out), ["planned", "planned"]);
    assert!(w.gh.writes().is_empty() && w.gl.writes().is_empty());
}

#[tokio::test]
async fn missing_tag_is_pending_and_writes_nothing() {
    let w = World::new().await;
    w.st.lock().unwrap().l_tags.clear();
    let out = w.sync(false).await;
    assert_eq!(status_of(&out, "gitlab"), "pending_tag");
    assert_eq!(status_of(&out, "github"), "synced");
    assert_eq!(out.exit, 75);
    assert!(w.gl.writes().is_empty());
}

#[tokio::test]
async fn ci_on_blocks_every_write() {
    let w = World::new().await;
    {
        let mut st = w.st.lock().unwrap();
        st.g_actions = true;
        st.l_builds = "enabled".into();
    }
    let out = w.sync(false).await;
    assert_eq!(statuses(&out), ["blocked_ci", "blocked_ci"]);
    assert_eq!(out.exit, 75);
    assert!(w.gh.writes().is_empty() && w.gl.writes().is_empty());
}

#[tokio::test]
async fn other_content_is_never_replaced() {
    let w = World::new().await;
    w.sync(false).await;
    {
        let mut st = w.st.lock().unwrap();
        let id = st.g_releases[0]["id"].as_u64().unwrap();
        st.g_assets
            .get_mut(&id)
            .unwrap()
            .get_mut("tool-linux")
            .unwrap()
            .digest = Some(format!("sha256:{}", "0".repeat(64)));
        st.l_files
            .insert("tool-linux.tar.gz".into(), vec![b"something else".to_vec()]);
    }
    let before = (w.gh.writes().len(), w.gl.writes().len());
    let out = w.sync(false).await;
    assert_eq!(statuses(&out), ["failed", "failed"]);
    assert_eq!(out.exit, 1);
    assert_eq!((w.gh.writes().len(), w.gl.writes().len()), before);
}

#[tokio::test]
async fn incomplete_github_stub_is_removed_and_uploaded_again() {
    let w = World::new().await;
    w.st.lock().unwrap().g_truncate = true;
    let out = w.sync(false).await;
    assert_eq!(status_of(&out, "github"), "failed"); // the fake keeps truncating: the upload is not accepted
    let rel = {
        let mut st = w.st.lock().unwrap();
        st.g_truncate = false;
        let rel = st.g_releases[0]["id"].as_u64().unwrap();
        st.g_assets.get_mut(&rel).unwrap().insert(
            "tool-linux".into(),
            GAsset {
                id: 999,
                name: "tool-linux".into(),
                state: "starter".into(),
                size: 0,
                digest: None,
                data: vec![],
            },
        );
        rel
    };
    let _ = rel;
    let out = w.sync(false).await;
    assert_eq!(status_of(&out, "github"), "synced", "{out:?}");
    assert_eq!(w.gh_stored(), w.st.lock().unwrap().files);
    assert!(w
        .gh
        .all()
        .iter()
        .any(|r| r.method == "DELETE" && r.path.ends_with("/assets/999")));
}

#[tokio::test]
async fn checksum_contract_is_enforced_before_any_write() {
    let w = World::new().await;
    {
        let mut st = w.st.lock().unwrap();
        st.files
            .insert("extra.bin".into(), b"no entry for this one".to_vec());
        st.served = st.files.clone();
    }
    let out = w.sync(false).await;
    assert_eq!(out.exit, 1);
    assert!(out.detail.unwrap().contains("extra.bin has no entry"));
    assert!(w.gh.all().is_empty() && w.gl.all().is_empty());
}

#[tokio::test]
async fn a_primary_that_does_not_match_its_sums_writes_nothing() {
    let w = World::new().await;
    w.st.lock()
        .unwrap()
        .served
        .insert("tool-linux.tar.gz".into(), b"corrupted".repeat(100));
    let out = w.sync(false).await;
    assert_eq!(statuses(&out), ["failed", "failed"]);
    assert!(w.gh.writes().is_empty() && w.gl.writes().is_empty());
}

#[tokio::test]
async fn tag_on_another_commit_is_refused() {
    let w = World::new().await;
    w.st.lock()
        .unwrap()
        .g_tags
        .insert(w.tag.clone(), "f".repeat(40));
    let out = w.sync(false).await;
    assert_eq!(status_of(&out, "github"), "failed");
    assert!(out.copies["github"].detail.contains("never moved"));
    assert!(w.gh.writes().is_empty());
}

#[tokio::test]
async fn rate_limit_stops_the_copy_without_retry() {
    let w = World::new().await;
    w.st.lock().unwrap().g_limit = true;
    let out = w.sync(false).await;
    assert_eq!(status_of(&out, "github"), "rate_limited");
    assert_eq!(out.exit, 75);
    assert_eq!(w.gh.all().iter().filter(|r| r.method == "POST").count(), 1);
}

#[tokio::test]
async fn draft_and_pull_mirror_and_foreign_orgs_do_nothing() {
    let w = World::new().await;
    w.st.lock().unwrap().draft = true;
    assert!(w
        .sync(false)
        .await
        .detail
        .unwrap()
        .contains("no published release"));
    {
        let mut st = w.st.lock().unwrap();
        st.draft = false;
        st.mirror = true;
    }
    assert!(w.sync(false).await.detail.unwrap().contains("pull mirror"));
    let mut cfg_json = json!({
        "forge_api": format!("{}/api/v1", w.forge.url), "forge_web": w.forge.url,
        "github": {"api": w.gh.url, "uploads": w.gh.url, "web": format!("{}/web", w.gh.url), "orgs": [], "min_write_interval_s": 0},
        "gitlab": {"api": format!("{}/api/v4", w.gl.url), "groups": []},
    });
    cfg_json["listeners"] = json!({});
    w.st.lock().unwrap().mirror = false;
    let ctx = cfrg_serve::release::Ctx::new(
        serde_json::from_value(cfg_json).unwrap(),
        cfrg_serve::config::Credentials::from_pairs(&[
            ("github_token", "x"),
            ("gitlab_token", "y"),
        ]),
    )
    .unwrap();
    let out = cfrg_serve::release::sync(&ctx, &w.repo, &w.tag, false).await;
    assert_eq!(statuses(&out), ["skipped", "skipped"]);
    assert!(w.gh.all().is_empty() && w.gl.all().is_empty());
}

#[tokio::test]
async fn flagged_github_org_is_left_alone() {
    let w = World::new().await;
    w.st.lock().unwrap().g_org_page = 404;
    let out = w.sync(false).await;
    assert_eq!(status_of(&out, "github"), "skipped");
    assert!(w.gh.writes().is_empty());
}

#[tokio::test]
async fn existing_gitlab_release_gets_its_missing_links() {
    let w = World::new().await;
    w.sync(false).await;
    w.st.lock()
        .unwrap()
        .l_links
        .retain(|l| l["name"] != "tool.receipt.json");
    let out = w.sync(false).await;
    assert_eq!(status_of(&out, "gitlab"), "synced", "{out:?}");
    assert!(w
        .st
        .lock()
        .unwrap()
        .l_links
        .iter()
        .any(|l| l["name"] == "tool.receipt.json"));
}

#[tokio::test]
async fn published_github_notes_follow_the_primary() {
    let w = World::new().await;
    w.sync(false).await;
    w.st.lock().unwrap().g_releases[0]["body"] = json!("stale notes");
    let out = w.sync(false).await;
    assert_eq!(status_of(&out, "github"), "synced");
    assert_eq!(
        w.st.lock().unwrap().g_releases[0]["body"],
        "Notes\r\nline two"
    );
}

#[tokio::test]
async fn unexpected_shapes_are_refused() {
    let w = World::new().await;
    assert_eq!(
        cfrg_serve::release::sync(&w.ctx, "../x", "v1", false)
            .await
            .exit,
        1
    );
    assert_eq!(
        cfrg_serve::release::sync(&w.ctx, "a/b", "v1;rm", false)
            .await
            .exit,
        1
    );
}

#[tokio::test]
async fn a_release_with_more_than_one_page_of_assets_syncs_and_reruns_as_a_noop() {
    // The first live run (45 assets) read only 20 release links; gitlab.com pages at 20 unless asked for more.
    let w = World::with(|st| {
        st.files = (0..45)
            .map(|n| {
                (
                    format!("asset-{n:02}.bin"),
                    format!("payload {n}").repeat(10).into_bytes(),
                )
            })
            .collect();
        add_sums(&mut st.files);
        st.served = st.files.clone();
    })
    .await;
    let first = w.sync(false).await;
    assert_eq!(statuses(&first), ["synced", "synced"], "{first:?}");
    assert_eq!(w.st.lock().unwrap().l_links.len(), 46);
    let writes = (w.gh.writes().len(), w.gl.writes().len());
    let again = w.sync(false).await;
    assert_eq!(statuses(&again), ["in_sync", "in_sync"], "{again:?}");
    assert_eq!((w.gh.writes().len(), w.gl.writes().len()), writes);
}

#[tokio::test]
async fn credential_names_are_one_hmac_key_per_forge() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["hmac_forgejo", "hmac_github", "webhook_secret", "unrelated"] {
        std::fs::write(dir.path().join(name), "value\n").unwrap();
    }
    let creds = cfrg_serve::config::Credentials::load(dir.path());
    assert_eq!(creds.names(), ["hmac_forgejo", "hmac_github"]);
}

/// R10, R11: the release of a contributed project is refused, whatever the request says, with no byte written and no
/// request made to any copy.
#[tokio::test]
async fn a_contributed_repositorys_release_is_refused() {
    let w = World::new().await;
    let dir = tempfile::tempdir().unwrap();
    let placement = dir.path().join("placement.json");
    std::fs::write(
        &placement,
        r#"{"native":{"schema":1,"source":{"origin":"https://forge.corbet.ch","token_env":"T"},"repositories":[],
            "contributed":[{"name":"ccvl-upstream","upstream":"https://forge.corbet.ch/corbet-fsl/ccvl"}]}}"#,
    )
    .unwrap();
    let cfg = json!({
        "forge_api": format!("{}/api/v1", w.forge.url), "forge_web": w.forge.url,
        "github": {"api": w.gh.url, "uploads": w.gh.url, "orgs": ["corbet-fsl"], "min_write_interval_s": 0},
        "gitlab": {"api": format!("{}/api/v4", w.gl.url), "groups": ["corbet-fsl"]},
        "placement": placement,
    });
    let ctx = cfrg_serve::release::Ctx::new(
        serde_json::from_value(cfg).unwrap(),
        cfrg_serve::config::Credentials::from_pairs(&[
            ("github_token", "x"),
            ("gitlab_token", "y"),
        ]),
    )
    .unwrap();
    for host_spelling in ["corbet-fsl/ccvl"] {
        let out = cfrg_serve::release::sync(&ctx, host_spelling, &w.tag, false).await;
        assert_eq!(out.exit, 0);
        let detail = out.detail.unwrap();
        assert!(
            detail.contains("release upload refused") && detail.contains("contributed project"),
            "{detail}"
        );
        assert!(out.copies.is_empty());
    }
    assert!(w.forge.all().is_empty(), "the primary is not even read");
    assert!(w.gh.all().is_empty() && w.gl.all().is_empty());
}

/// A copy that is a contributed project (a fork of someone else's project) is refused per copy.
#[tokio::test]
async fn a_contributed_copy_is_refused_and_the_other_copy_still_syncs() {
    let w = World::new().await;
    let dir = tempfile::tempdir().unwrap();
    let placement = dir.path().join("placement.json");
    std::fs::write(
        &placement,
        r#"{"native":{"schema":1,"source":{"origin":"https://forge.corbet.ch","token_env":"T"},"repositories":[],
            "contributed":[{"name":"theirs","upstream":"https://github.com/corbet-fsl/ccvl"}]}}"#,
    )
    .unwrap();
    let cfg = json!({
        "forge_api": format!("{}/api/v1", w.forge.url), "forge_web": w.forge.url,
        "github": {"api": w.gh.url, "uploads": w.gh.url, "web": format!("{}/web", w.gh.url), "orgs": ["corbet-fsl"], "min_write_interval_s": 0},
        "gitlab": {"api": format!("{}/api/v4", w.gl.url), "groups": ["corbet-fsl"]},
        "placement": placement,
    });
    let ctx = cfrg_serve::release::Ctx::new(
        serde_json::from_value(cfg).unwrap(),
        cfrg_serve::config::Credentials::from_pairs(&[
            ("github_token", "x"),
            ("gitlab_token", "y"),
        ]),
    )
    .unwrap();
    let out = cfrg_serve::release::sync(&ctx, &w.repo, &w.tag, false).await;
    assert_eq!(status_of(&out, "github"), "refused", "{out:?}");
    assert_eq!(status_of(&out, "gitlab"), "synced");
    assert!(w.gh.all().is_empty());
}

#[tokio::test]
async fn one_copy_can_be_synced_alone() {
    let w = World::new().await;
    let out = cfrg_serve::release::sync_for(&w.ctx, &w.repo, &w.tag, false, Some("gitlab")).await;
    assert_eq!(
        out.copies.keys().cloned().collect::<Vec<_>>(),
        ["bitbucket", "gitlab"]
    );
    assert!(w.gh.all().is_empty());
}
