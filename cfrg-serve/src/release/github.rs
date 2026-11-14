//! The GitHub copy: draft release, assets one at a time, publish, read back.
use super::{
    api::{Api, Pacer, Payload},
    log, norm_text, quote, result, written, CopyResult, Ctx, Primary, Stop, SUM_FILES,
};
use reqwest::Method;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

fn http_error(what: &str, status: u16) -> CopyResult {
    result("failed", format!("{what} answered HTTP {status}"))
}

async fn asset_list(
    api: &Api,
    repo: &str,
    release_id: u64,
) -> Result<BTreeMap<String, Value>, Stop> {
    let mut far = BTreeMap::new();
    for page in 1..=5 {
        let (status, listing) = api
            .get_json(&format!(
                "/repos/{repo}/releases/{release_id}/assets?per_page=100&page={page}"
            ))
            .await?;
        let Some(Value::Array(items)) =
            listing.filter(|l| status == 200 && l.as_array().is_some_and(|a| !a.is_empty()))
        else {
            break;
        };
        for item in items {
            if let Some(name) = item["name"].as_str() {
                far.insert(name.to_owned(), item.clone());
            }
        }
    }
    Ok(far)
}

/// The bearer for the copy: a plain `github-token` if one is mounted, else the GitHub App's installation token for the
/// copy's owner (the App is installed per organisation; `github-app-id` and `github-app-key`). `None`: no credential at all.
async fn github_token(ctx: &Ctx, api: &str, repo: &str) -> Result<Option<String>, String> {
    if let Some(t) = ctx.creds.get("github_token") {
        return Ok(Some(t.to_owned()));
    }
    let (Some(id), Some(key)) = (
        ctx.creds.get("github_app_id"),
        ctx.creds.get("github_app_key"),
    ) else {
        return Ok(None);
    };
    let app_id: u64 = id
        .trim()
        .parse()
        .map_err(|_| "github-app-id is not a number".to_owned())?;
    let creds = cghb::relay::AppCredentials {
        app_id,
        private_key_pem: cghb::relay::Redacted::new(key),
    };
    let driver = cghb::relay::GithubRelay::new(&creds, api).map_err(|e| e.to_string())?;
    let target = cghb::relay::Repo::parse(repo).map_err(|e| e.to_string())?;
    let t = driver
        .installation_token(&target)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Some(t.expose().to_owned()))
}

pub async fn sync(ctx: &Ctx, primary: &mut Primary, dry: bool) -> Result<CopyResult, Stop> {
    let gh = &ctx.cfg.github;
    let repo = ctx.cfg.copy_path("github", &primary.repo);
    let owner = repo.split('/').next().unwrap_or_default().to_owned();
    if !gh.orgs.iter().any(|o| o.eq_ignore_ascii_case(&owner)) {
        return Ok(result(
            "skipped",
            format!(
                "organisation {owner} is not enabled for release upload (config.json github.orgs)"
            ),
        ));
    }
    let token = match github_token(ctx, &gh.api, &repo).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return Ok(result(
                "blocked_ci",
                "no GitHub credential in this deployment",
            ))
        }
        Err(why) => return Ok(result("failed", why)),
    };
    let bearer = format!("Bearer {token}");
    let headers = [
        ("accept", "application/vnd.github+json"),
        ("x-github-api-version", "2022-11-28"),
        ("authorization", bearer.as_str()),
    ];
    // One pacer for both hosts: the writes of a copy are spaced as a whole.
    let pacer = Arc::new(Pacer::new(gh.min_write_interval_s));
    let api = Api::new(
        ctx.public_client.clone(),
        &gh.api,
        &headers,
        Some(pacer.clone()),
    )?;
    let uploads = Api::new(
        ctx.public_client.clone(),
        &gh.uploads,
        &headers,
        Some(pacer),
    )?;

    let (status, info) = api.get_json(&format!("/repos/{repo}")).await?;
    if status == 404 {
        return Ok(result("no_copy", "no GitHub copy at this path"));
    }
    let Some(info) = info.filter(|_| status == 200) else {
        return Ok(result("failed", format!("GET repo -> HTTP {status}")));
    };
    if info["archived"].as_bool() == Some(true) {
        return Ok(result("skipped", "the copy is archived"));
    }
    if let Some(web) = &gh.web {
        let plain = Api::new(ctx.public_client.clone(), web, &[], None)?;
        let page = plain
            .call(Method::GET, &format!("/{owner}"), Payload::None, &[])
            .await?;
        if page.status == 404 {
            return Ok(result(
                "skipped",
                format!("organisation {owner} looks flagged (its page answers 404 while the API answers); automation stops there"),
            ));
        }
    }
    // The tag: never created here.
    let quoted = quote(&primary.tag);
    let (status, reference) = api
        .get_json(&format!("/repos/{repo}/git/ref/tags/{quoted}"))
        .await?;
    if status == 404 {
        return Ok(result(
            "pending_tag",
            "the tag is not on the GitHub copy yet; cfrg sends the hint again after it pushes it",
        ));
    }
    let reference = reference.unwrap_or(Value::Null);
    let mut commit = reference["object"]["sha"].as_str().map(str::to_owned);
    if reference["object"]["type"].as_str() == Some("tag") {
        if let Some(sha) = &commit {
            let (_, annotated) = api
                .get_json(&format!("/repos/{repo}/git/tags/{sha}"))
                .await?;
            commit = annotated.and_then(|t| t["object"]["sha"].as_str().map(str::to_owned));
        }
    }
    if let Some(expected) = &primary.commit {
        if commit.as_ref() != Some(expected) {
            return Ok(result(
                "failed",
                format!(
                    "the tag {} points at {} on the copy and at {expected} on the primary; tags are never moved",
                    primary.tag,
                    commit.as_deref().unwrap_or("nothing")
                ),
            ));
        }
    }
    // The release and its assets.
    let mut release: Option<Value> = None;
    for page in 1..=10 {
        let (status, listing) = api
            .get_json(&format!("/repos/{repo}/releases?per_page=100&page={page}"))
            .await?;
        let Some(Value::Array(items)) =
            listing.filter(|l| status == 200 && l.as_array().is_some_and(|a| !a.is_empty()))
        else {
            break;
        };
        release = items
            .into_iter()
            .find(|r| r["tag_name"].as_str() == Some(primary.tag.as_str()));
        if release.is_some() {
            break;
        }
    }
    let far = match release.as_ref().and_then(|r| r["id"].as_u64()) {
        Some(id) => asset_list(&api, &repo, id).await?,
        None => BTreeMap::new(),
    };
    let mut todo: Vec<(&str, String)> = Vec::new();
    for (name, size) in &primary.assets {
        let want = &primary.want[name];
        let Some(have) = far.get(name) else {
            todo.push(("upload", name.clone()));
            continue;
        };
        if have["state"].as_str() != Some("uploaded") {
            todo.push(("replace-stub", name.clone()));
            continue;
        }
        let digest = have["digest"].as_str().filter(|d| !d.is_empty());
        let expected = format!("sha256:{want}");
        if have["size"].as_u64() != Some(*size) || digest.is_some_and(|d| d != expected) {
            return Ok(result(
                "failed",
                format!("{name} exists on the copy with other content; assets are never replaced"),
            ));
        }
        if digest.is_none() {
            let id = have["id"].as_u64().unwrap_or(0);
            let (status, _, read) = api
                .download(
                    &format!("/repos/{repo}/releases/assets/{id}"),
                    None,
                    &[("accept", "application/octet-stream")],
                )
                .await?;
            if status != 200 || read != *want {
                return Ok(result(
                    "failed",
                    format!("{name} exists on the copy with other content (read back sha256 {read}); assets are never replaced"),
                ));
            }
        }
    }
    let meta_differs = release.as_ref().is_some_and(|r| {
        norm_text(r["body"].as_str().unwrap_or("")) != norm_text(primary.body())
            || r["name"].as_str().unwrap_or("") != primary.title()
            || (r["prerelease"].as_bool() == Some(true)) != primary.prerelease()
    });
    match &release {
        None => todo.insert(0, ("create-release", String::new())),
        Some(r) if r["draft"].as_bool() == Some(true) => todo.push(("publish", String::new())),
        Some(_) => {}
    }
    if meta_differs {
        todo.push(("update-notes", String::new()));
    }
    if todo.is_empty() {
        return Ok(written(
            result("in_sync", "release and all assets identical"),
            0,
        ));
    }
    if dry {
        let list: Vec<String> = todo
            .iter()
            .map(|(a, n)| format!("{a} {n}").trim().to_owned())
            .collect();
        return Ok(written(
            result("planned", format!("would write: {}", list.join(", "))),
            0,
        ));
    }
    // Gates before the first write (R6, R7): CI must be provably off on the copy.
    let (status, perms) = api
        .get_json(&format!("/repos/{repo}/actions/permissions"))
        .await?;
    if status != 200 || perms.is_none_or(|p| p["enabled"].as_bool() != Some(false)) {
        return Ok(result(
            "blocked_ci",
            "Actions is on (or its state cannot be read) on the GitHub copy; R6/R7: nothing is written until CI is off there",
        ));
    }
    if let Err(Stop::Contract(error)) = primary.verify_all().await {
        return Ok(result(
            "failed",
            format!("the primary does not verify: {error}"),
        ));
    }
    let writes_before = api.writes() + uploads.writes();
    let created = release.is_none();
    let mut release = match release {
        Some(r) => r,
        None => {
            let body = json!({
                "tag_name": primary.tag, "name": primary.title(), "body": primary.body(),
                "draft": true, "prerelease": primary.prerelease()});
            let (status, made) = api
                .json(
                    Method::POST,
                    &format!("/repos/{repo}/releases"),
                    Some(&body),
                )
                .await?;
            let Some(made) = made.filter(|_| status == 201) else {
                return Ok(http_error("creating the release", status));
            };
            log(
                "github-release-created",
                json!({"repo": repo, "tag": primary.tag}),
            );
            made
        }
    };
    let id = release["id"].as_u64().unwrap_or(0);
    let work = tempfile::Builder::new()
        .prefix("release-upload-")
        .tempdir()
        .map_err(|e| Stop::Io(e.to_string()))?;
    for (action, name) in &todo {
        if *action != "upload" && *action != "replace-stub" {
            continue;
        }
        if *action == "replace-stub" {
            let stub = far[name]["id"].as_u64().unwrap_or(0);
            api.call(
                Method::DELETE,
                &format!("/repos/{repo}/releases/assets/{stub}"),
                Payload::None,
                &[],
            )
            .await?;
            log("github-stub-removed", json!({"repo": repo, "asset": name}));
        }
        let path = work.path().join("asset");
        primary.fetch(name, &path).await?;
        let size = tokio::fs::metadata(&path)
            .await
            .map_err(|e| Stop::Io(e.to_string()))?
            .len();
        let response = uploads
            .call(
                Method::POST,
                &format!("/repos/{repo}/releases/{id}/assets?name={}", quote(name)),
                Payload::File(&path, size),
                &[("content-type", "application/octet-stream")],
            )
            .await?;
        let sent: Value = if response.status == 201 {
            serde_json::from_slice(&response.body).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let state_ok = sent["state"].is_null() || sent["state"].as_str() == Some("uploaded");
        let digest_ok = sent["digest"]
            .as_str()
            .filter(|d| !d.is_empty())
            .is_none_or(|d| d == format!("sha256:{}", primary.want[name]));
        if response.status != 201 || sent["size"].as_u64() != Some(size) || !state_ok || !digest_ok
        {
            return Ok(result(
                "failed",
                format!(
                    "uploading {name} answered HTTP {}, {} bytes, digest {}",
                    response.status,
                    sent["size"]
                        .as_u64()
                        .map_or("None".into(), |s| s.to_string()),
                    sent["digest"].as_str().unwrap_or("None")
                ),
            ));
        }
        log(
            "github-asset-uploaded",
            json!({"repo": repo, "asset": name, "bytes": size}),
        );
        let _ = tokio::fs::remove_file(&path).await;
    }
    if created || release["draft"].as_bool() == Some(true) {
        let (status, published) = api
            .json(
                Method::PATCH,
                &format!("/repos/{repo}/releases/{id}"),
                Some(&json!({"draft": false})),
            )
            .await?;
        let Some(published) = published.filter(|_| status == 200) else {
            return Ok(http_error("publishing the release", status));
        };
        release = published;
    }
    if meta_differs {
        let body = json!({"name": primary.title(), "body": primary.body(), "prerelease": primary.prerelease()});
        api.json(
            Method::PATCH,
            &format!(
                "/repos/{repo}/releases/{}",
                release["id"].as_u64().unwrap_or(id)
            ),
            Some(&body),
        )
        .await?;
    }
    let release_id = release["id"].as_u64().unwrap_or(id);
    verify(
        &api,
        primary,
        &repo,
        release_id,
        api.writes() + uploads.writes() - writes_before,
    )
    .await
}

async fn verify(
    api: &Api,
    primary: &Primary,
    repo: &str,
    release_id: u64,
    writes: u64,
) -> Result<CopyResult, Stop> {
    let far = asset_list(api, repo, release_id).await?;
    for (name, size) in &primary.assets {
        let want = format!("sha256:{}", primary.want[name]);
        let ok = far.get(name).is_some_and(|have| {
            have["state"].as_str() == Some("uploaded")
                && have["size"].as_u64() == Some(*size)
                && have["digest"]
                    .as_str()
                    .filter(|d| !d.is_empty())
                    .is_none_or(|d| d == want)
        });
        if !ok {
            return Ok(written(
                result(
                    "failed",
                    format!("read back: {name} is not on the copy as the primary has it"),
                ),
                writes,
            ));
        }
    }
    if let Some(sums) = primary
        .assets
        .keys()
        .find(|n| SUM_FILES.contains(&n.as_str()))
    {
        let id = far[sums]["id"].as_u64().unwrap_or(0);
        let (status, _, digest) = api
            .download(
                &format!("/repos/{repo}/releases/assets/{id}"),
                None,
                &[("accept", "application/octet-stream")],
            )
            .await?;
        if status != 200 || digest != primary.want[sums] {
            return Ok(written(
                result(
                    "failed",
                    format!("read back: {sums} on the copy differs from the primary's"),
                ),
                writes,
            ));
        }
    }
    let mut done = written(
        result("synced", "release and all assets identical, read back"),
        writes,
    );
    done.assets = Some(primary.assets.len());
    Ok(done)
}
