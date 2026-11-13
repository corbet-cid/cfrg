//! The primary (Forgejo): the release, its checksum contract, and the bytes.
use super::{quote, valid_asset_name, Api, Ctx, Stop, SMALL, SUM_FILES};
use reqwest::Method;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub struct Primary {
    pub repo: String,
    pub tag: String,
    api: Api,
    web: Api,
    pub release: Value,
    /// name -> size on the primary.
    pub assets: BTreeMap<String, u64>,
    /// name -> sha256 the contract demands, for EVERY asset.
    pub want: BTreeMap<String, String>,
    pub commit: Option<String>,
    bytes: BTreeMap<String, Vec<u8>>,
    verified: bool,
}

fn is_sum_file(name: &str) -> bool {
    SUM_FILES.contains(&name) || name.ends_with(".sha256") || name.ends_with(".sha256sum")
}

impl Primary {
    pub fn new(ctx: &Ctx, repo: &str, tag: &str) -> Self {
        let token = ctx.creds.get("forge_token").map(|t| format!("token {t}"));
        let auth: Vec<(&str, &str)> = token
            .iter()
            .map(|t| ("authorization", t.as_str()))
            .collect();
        // The constructor only fails on a header value that is not text, which a token never is.
        let make = |base: &str| {
            Api::new(ctx.forge_client.clone(), base, &auth, None)
                .unwrap_or_else(|e| unreachable!("static headers: {e}"))
        };
        Self {
            repo: repo.to_owned(),
            tag: tag.to_owned(),
            api: make(&ctx.cfg.forge_api),
            web: make(&ctx.cfg.forge_web),
            release: Value::Null,
            assets: BTreeMap::new(),
            want: BTreeMap::new(),
            commit: None,
            bytes: BTreeMap::new(),
            verified: false,
        }
    }

    /// `Ok(None)` when the release is ready to mirror, `Ok(Some(reason))` when there is nothing to mirror.
    pub async fn read(&mut self) -> Result<Option<String>, Stop> {
        let (status, info) = self.api.get_json(&format!("/repos/{}", self.repo)).await?;
        let Some(info) = info.filter(|_| status == 200) else {
            return Ok(Some(format!(
                "the repository is not readable on the primary (HTTP {status}; a private one needs the forge read token)"
            )));
        };
        if info["mirror"].as_bool() == Some(true) {
            return Ok(Some(
                "a pull mirror of someone else's project: releases of contributed projects are not relayed (R11)".into(),
            ));
        }
        let tag = quote(&self.tag);
        let (status, release) = self
            .api
            .get_json(&format!("/repos/{}/releases/tags/{tag}", self.repo))
            .await?;
        let Some(release) = release.filter(|r| status == 200 && r["draft"].as_bool() != Some(true))
        else {
            return Ok(Some("no published release for this tag".into()));
        };
        for asset in release["assets"].as_array().into_iter().flatten() {
            let name = asset["name"].as_str().unwrap_or_default();
            if !valid_asset_name(name) {
                return Err(Stop::Contract(format!(
                    "asset name '{name}' has a shape the copies do not take"
                )));
            }
            self.assets
                .insert(name.to_owned(), asset["size"].as_u64().unwrap_or(0));
        }
        self.release = release;
        let (status, found) = self
            .api
            .get_json(&format!("/repos/{}/tags/{tag}", self.repo))
            .await?;
        self.commit = found
            .filter(|_| status == 200)
            .and_then(|t| t["commit"]["sha"].as_str().map(str::to_owned));
        self.want = self.contract().await?;
        Ok(None)
    }

    fn download_path(&self, name: &str) -> String {
        format!(
            "/{}/releases/download/{}/{}",
            self.repo,
            quote(&self.tag),
            quote(name)
        )
    }

    async fn small(&self, name: &str) -> Result<Vec<u8>, Stop> {
        let size = self.assets[name];
        if size > SMALL {
            return Err(Stop::Contract(format!(
                "checksum file {name} is {size} bytes"
            )));
        }
        let response = self
            .web
            .call(
                Method::GET,
                &self.download_path(name),
                super::api::Payload::None,
                &[],
            )
            .await?;
        if response.status != 200 || response.body.len() as u64 != size {
            return Err(Stop::Contract(format!(
                "checksum file {name} could not be read in full (HTTP {}, {} of {size} bytes)",
                response.status,
                response.body.len()
            )));
        }
        Ok(response.body.to_vec())
    }

    /// name -> sha256 for EVERY asset. Checksum files vouch for the others; their own digest is that of the bytes read.
    async fn contract(&mut self) -> Result<BTreeMap<String, String>, Stop> {
        let sums: Vec<String> = self
            .assets
            .keys()
            .filter(|n| is_sum_file(n))
            .cloned()
            .collect();
        let mut listed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for name in &sums {
            let data = self.small(name).await?;
            for line in String::from_utf8_lossy(&data).lines() {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let Some(digest) = fields
                    .first()
                    .filter(|f| f.len() == 64 && f.bytes().all(|b| b.is_ascii_hexdigit()))
                else {
                    continue;
                };
                let target = if name.ends_with(".sha256") || name.ends_with(".sha256sum") {
                    match fields.get(1) {
                        Some(f) => f.trim_start_matches('*').to_owned(),
                        None => name
                            .rsplit_once('.')
                            .map_or(name.as_str(), |(base, _)| base)
                            .to_owned(),
                    }
                } else {
                    match fields.get(1) {
                        Some(f) => f.trim_start_matches('*').to_owned(),
                        None => continue,
                    }
                };
                let target = target.strip_prefix("./").unwrap_or(&target).to_owned();
                listed
                    .entry(target)
                    .or_default()
                    .insert(digest.to_ascii_lowercase());
            }
            self.bytes.insert(name.clone(), data);
        }
        let others: Vec<&String> = self.assets.keys().filter(|n| !sums.contains(n)).collect();
        if !others.is_empty() && sums.is_empty() {
            return Err(Stop::Contract(
                "the release has assets but no checksum file".into(),
            ));
        }
        let mut want = BTreeMap::new();
        for name in others {
            let digests = listed.get(name.as_str()).cloned().unwrap_or_default();
            match digests.len() {
                0 => {
                    return Err(Stop::Contract(format!(
                        "{name} has no entry in the release's checksum files"
                    )))
                }
                1 => {
                    want.insert(name.clone(), digests.into_iter().next().unwrap_or_default());
                }
                _ => {
                    return Err(Stop::Contract(format!(
                        "{name} has conflicting checksum entries"
                    )))
                }
            }
        }
        for name in &sums {
            let digest = hex::encode(Sha256::digest(&self.bytes[name]));
            if let Some(entry) = listed.get(name.as_str()) {
                if *entry != BTreeSet::from([digest.clone()]) {
                    return Err(Stop::Contract(format!(
                        "{name} does not match the entry another checksum file lists for it"
                    )));
                }
            }
            want.insert(name.clone(), digest);
        }
        Ok(want)
    }

    /// Read every asset from the primary once and compare size and sha256 with the contract; nothing is written anywhere.
    pub async fn verify_all(&mut self) -> Result<(), Stop> {
        if self.verified {
            return Ok(());
        }
        for (name, size) in &self.assets {
            let (status, got, digest) = self
                .web
                .download(&self.download_path(name), None, &[])
                .await?;
            if status != 200 {
                return Err(Stop::Contract(format!(
                    "{name}: the primary answered HTTP {status}"
                )));
            }
            if got != *size || digest != self.want[name] {
                return Err(Stop::Contract(format!(
                    "{name}: {got} bytes sha256 {digest}, the contract says {size} bytes {}",
                    self.want[name]
                )));
            }
        }
        self.verified = true;
        super::log(
            "primary-verified",
            serde_json::json!({"repo": self.repo, "tag": self.tag, "assets": self.assets.len()}),
        );
        Ok(())
    }

    pub async fn fetch(&self, name: &str, dest: &Path) -> Result<(), Stop> {
        let (status, size, digest) = self
            .web
            .download(&self.download_path(name), Some(dest), &[])
            .await?;
        if status != 200 || digest != self.want[name] || size != self.assets[name] {
            return Err(Stop::Contract(format!(
                "{name}: the primary changed while it was copied (HTTP {status}, sha256 {digest})"
            )));
        }
        Ok(())
    }

    pub fn body(&self) -> &str {
        self.release["body"].as_str().unwrap_or("")
    }

    pub fn title(&self) -> String {
        match self.release["name"].as_str() {
            Some(n) if !n.is_empty() => n.to_owned(),
            _ => self.tag.clone(),
        }
    }

    pub fn prerelease(&self) -> bool {
        self.release["prerelease"].as_bool() == Some(true)
    }
}
