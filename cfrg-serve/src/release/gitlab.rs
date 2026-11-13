//! The GitLab copy: generic package files, then the release that links them, read back.
use super::{
    api::{Api, Payload},
    log, norm_text, paged, quote, result, written, CopyResult, Ctx, Primary, Stop, SUM_FILES,
};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// file name -> [(size, sha256)] of the generic package `pkg` at version `tag`, as GitLab computed them.
async fn stored(
    api: &Api,
    base: &str,
    pkg: &str,
    tag: &str,
) -> Result<BTreeMap<String, Vec<(u64, String)>>, Stop> {
    let mut files: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
    for package in paged(
        api,
        &format!(
            "{base}/packages?package_type=generic&package_name={}",
            quote(pkg)
        ),
        20,
    )
    .await?
    {
        if package["name"].as_str() == Some(pkg) && package["version"].as_str() == Some(tag) {
            let id = package["id"].as_u64().unwrap_or(0);
            for item in paged(api, &format!("{base}/packages/{id}/package_files"), 20).await? {
                if let Some(name) = item["file_name"].as_str() {
                    files.entry(name.to_owned()).or_default().push((
                        item["size"].as_u64().unwrap_or(0),
                        item["file_sha256"].as_str().unwrap_or("").to_owned(),
                    ));
                }
            }
        }
    }
    Ok(files)
}

pub async fn sync(ctx: &Ctx, primary: &mut Primary, dry: bool) -> Result<CopyResult, Stop> {
    let gl = &ctx.cfg.gitlab;
    let repo = ctx.cfg.copy_path("gitlab", &primary.repo);
    let group = repo.split('/').next().unwrap_or_default();
    if !gl.groups.iter().any(|g| g.eq_ignore_ascii_case(group)) {
        return Ok(result(
            "skipped",
            format!("group {group} is not enabled for release upload (config.json gitlab.groups)"),
        ));
    }
    let Some(token) = ctx.creds.get("gitlab_token") else {
        return Ok(result(
            "blocked_ci",
            "no GitLab credential in this deployment",
        ));
    };
    let api = Api::new(
        ctx.public_client.clone(),
        &gl.api,
        &[("private-token", token)],
        None,
    )?;
    let (status, project) = api.get_json(&format!("/projects/{}", quote(&repo))).await?;
    if status == 404 {
        return Ok(result("no_copy", "no GitLab copy at this path"));
    }
    let Some(project) = project.filter(|_| status == 200) else {
        return Ok(result("failed", format!("GET project -> HTTP {status}")));
    };
    if project["archived"].as_bool() == Some(true) {
        return Ok(result("skipped", "the copy is archived"));
    }
    let pid = project["id"].as_u64().unwrap_or(0);
    let pkg = project["path"].as_str().unwrap_or_default().to_owned();
    let base = format!("/projects/{pid}");
    let quoted = quote(&primary.tag);
    // The tag: never created here.
    let (status, tag) = api
        .get_json(&format!("{base}/repository/tags/{quoted}"))
        .await?;
    if status == 404 {
        return Ok(result(
            "pending_tag",
            "the tag is not on the GitLab copy yet; cfrg sends the hint again after it pushes it",
        ));
    }
    let Some(tag) = tag.filter(|_| status == 200) else {
        return Ok(result("failed", format!("GET tag -> HTTP {status}")));
    };
    let commit = tag["commit"]["id"].as_str().map(str::to_owned);
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
    let have_files = stored(&api, &base, &pkg, &primary.tag).await?;
    let mut todo: Vec<(&str, String)> = Vec::new();
    for (name, size) in &primary.assets {
        match have_files.get(name) {
            None => todo.push(("upload", name.clone())),
            Some(have) if !have.contains(&(*size, primary.want[name].clone())) => {
                return Ok(result(
                    "failed",
                    format!(
                        "{name} exists on the copy with other content; assets are never replaced"
                    ),
                ));
            }
            Some(_) => {}
        }
    }
    let link_base = format!(
        "{}/projects/{pid}/packages/generic/{pkg}/{}",
        gl.api.trim_end_matches('/'),
        primary.tag
    );
    let (status, release) = api.get_json(&format!("{base}/releases/{quoted}")).await?;
    let mut links: BTreeMap<String, Value> = BTreeMap::new();
    let release = match (status, release) {
        (200, Some(release)) => {
            for item in paged(&api, &format!("{base}/releases/{quoted}/assets/links"), 20).await? {
                if let Some(name) = item["name"].as_str() {
                    links.insert(name.to_owned(), item.clone());
                }
            }
            Some(release)
        }
        (404, _) => None,
        (other, _) => return Ok(result("failed", format!("GET release -> HTTP {other}"))),
    };
    let description = if primary.prerelease() {
        format!("Pre-release.\n\n{}", primary.body())
    } else {
        primary.body().to_owned()
    };
    match &release {
        None => todo.push(("create-release", String::new())),
        Some(r) => {
            for name in primary.assets.keys().filter(|n| !links.contains_key(*n)) {
                todo.push(("link", name.clone()));
            }
            if norm_text(r["description"].as_str().unwrap_or("")) != norm_text(&description)
                || r["name"].as_str().unwrap_or("") != primary.title()
            {
                todo.push(("update-notes", String::new()));
            }
        }
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
    // Gate (R6, R7): CI must be provably off on the copy.
    if project["builds_access_level"].as_str() != Some("disabled")
        || project["jobs_enabled"].as_bool() == Some(true)
    {
        return Ok(result(
            "blocked_ci",
            "CI/CD is on on the GitLab copy; R6/R7: nothing is written until it is off there",
        ));
    }
    if let Err(Stop::Contract(error)) = primary.verify_all().await {
        return Ok(result(
            "failed",
            format!("the primary does not verify: {error}"),
        ));
    }
    let writes_before = api.writes();
    let work = tempfile::Builder::new()
        .prefix("release-upload-")
        .tempdir()
        .map_err(|e| Stop::Io(e.to_string()))?;
    for (action, name) in &todo {
        if *action != "upload" {
            continue;
        }
        let path = work.path().join("asset");
        primary.fetch(name, &path).await?;
        let size = tokio::fs::metadata(&path)
            .await
            .map_err(|e| Stop::Io(e.to_string()))?
            .len();
        let response = api
            .call(
                Method::PUT,
                &format!(
                    "{base}/packages/generic/{}/{quoted}/{}?status=default",
                    quote(&pkg),
                    quote(name)
                ),
                Payload::File(&path, size),
                &[("content-type", "application/octet-stream")],
            )
            .await?;
        if response.status != 200 && response.status != 201 {
            return Ok(result(
                "failed",
                format!("uploading {name} answered HTTP {}", response.status),
            ));
        }
        log(
            "gitlab-asset-uploaded",
            json!({"repo": repo, "asset": name, "bytes": size}),
        );
        let _ = tokio::fs::remove_file(&path).await;
    }
    let link = |name: &str| json!({"name": name, "url": format!("{link_base}/{}", quote(name)), "link_type": "package"});
    if release.is_none() {
        let links: Vec<Value> = primary.assets.keys().map(|n| link(n)).collect();
        let body = json!({"tag_name": primary.tag, "name": primary.title(), "description": description, "assets": {"links": links}});
        let (status, _) = api
            .json(Method::POST, &format!("{base}/releases"), Some(&body))
            .await?;
        if status != 201 {
            return Ok(result(
                "failed",
                format!("creating the release answered HTTP {status}"),
            ));
        }
    } else {
        for (action, name) in &todo {
            if *action == "link" {
                let (status, _) = api
                    .json(
                        Method::POST,
                        &format!("{base}/releases/{quoted}/assets/links"),
                        Some(&link(name)),
                    )
                    .await?;
                if status != 201 {
                    return Ok(result(
                        "failed",
                        format!("linking {name} answered HTTP {status}"),
                    ));
                }
            }
        }
        if todo.iter().any(|(a, _)| *a == "update-notes") {
            let body = json!({"name": primary.title(), "description": description});
            api.json(
                Method::PUT,
                &format!("{base}/releases/{quoted}"),
                Some(&body),
            )
            .await?;
        }
    }
    verify(
        &api,
        primary,
        pid,
        &pkg,
        &link_base,
        api.writes() - writes_before,
    )
    .await
}

async fn verify(
    api: &Api,
    primary: &Primary,
    pid: u64,
    pkg: &str,
    link_base: &str,
    writes: u64,
) -> Result<CopyResult, Stop> {
    let base = format!("/projects/{pid}");
    let files = stored(api, &base, pkg, &primary.tag).await?;
    for (name, size) in &primary.assets {
        if !files
            .get(name)
            .is_some_and(|have| have.contains(&(*size, primary.want[name].clone())))
        {
            return Ok(written(
                result(
                    "failed",
                    format!("read back: {name} is not stored as the primary has it"),
                ),
                writes,
            ));
        }
    }
    let have: Vec<String> = paged(
        api,
        &format!("{base}/releases/{}/assets/links", quote(&primary.tag)),
        20,
    )
    .await?
    .iter()
    .filter_map(|i| i["name"].as_str().map(str::to_owned))
    .collect();
    let missing: Vec<&str> = primary
        .assets
        .keys()
        .filter(|n| !have.contains(n))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Ok(written(
            result(
                "failed",
                format!(
                    "read back: the release lacks links for {}",
                    missing.join(", ")
                ),
            ),
            writes,
        ));
    }
    if let Some(sums) = primary
        .assets
        .keys()
        .find(|n| SUM_FILES.contains(&n.as_str()))
    {
        let (status, _, digest) = api
            .download(&format!("{link_base}/{}", quote(sums)), None, &[])
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
