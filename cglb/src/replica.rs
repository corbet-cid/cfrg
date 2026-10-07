//! GitLab as one repository of a replica set: reads, native remote mirrors as
//! a sender, and deploy-key or user protection as a receiver.
//!
//! Verified live on GitLab Free (see `docs/switch.md`): remote mirrors accept
//! HTTPS credentials in the URL (the API masks them), run within minutes or
//! at once through `sync`, and a protected branch can be locked to a deploy
//! key only. Free has no per-user push allow-list, so a receiver is locked to
//! the sender's mirror KEY; a sender without keys cannot be admitted here.
use crate::native::{call, rules};
use cfrg::{
    land::{Capability, Support},
    model::Forge,
    native::{
        encode,
        http::{expect, Transport},
        Destination, Repository,
    },
    replicate::{address_of, secret, Admit, Capabilities, Mirror, Refs, Replica, Site, Target},
    Result,
};
use serde_json::{json, Value};

pub const CAPABILITIES: Capabilities = Capabilities {
    forge: Forge::Gitlab,
    push_mirror: Capability {
        support: Support::Native,
        note: "native: project remote mirrors (Free), HTTPS credentials or a GitLab-generated SSH key, at most 10 per project, run within minutes of a push and at once through the sync call; divergent refs are overwritten",
    },
    mirror_key: false,
    pull_mirror: Capability {
        support: Support::Unsupported,
        note: "unsupported on Free: pull mirrors are Premium",
    },
    pull_mirror_converts_existing: false,
    receiver_lock: Capability {
        support: Support::NativeFill,
        note: "native: protected branch locked to one deploy key; Free has no per-user allow-list, so only a mirror key can be admitted exclusively; cfrg fills: delete and recreate rules, because the PATCH of allowed_to_push is refused",
    },
    switch: Capability {
        support: Support::NativeFill,
        note: "native: remote mirrors and protected branches; cfrg fills the order (freeze, hash check, disable, unlock, create, verify)",
    },
    rename: Capability {
        support: Support::Native,
        note: "native: updating the project path (and name) inside its namespace; GitLab redirects the old path (301), so cfrg addresses the project by ID and re-points the Forgejo push mirror; a namespace change is a transfer and is not implemented",
    },
};

pub struct Gitlab {
    site: Site,
    repo: Repository,
    dest: Destination,
}

impl Gitlab {
    pub fn new(repo: &Repository, dest: &Destination) -> Self {
        let host = dest.endpoint.origin.trim_start_matches("https://");
        Self {
            site: Site::new(
                Forge::Gitlab,
                &dest.endpoint.origin,
                &dest.endpoint.origin,
                host,
                &dest.path,
                dest.repository_id.clone(),
            ),
            repo: repo.clone(),
            dest: dest.clone(),
        }
    }

    /// Deploy keys cfrg enrolled for mirrors that no longer exist (their private
    /// half died with the mirror) are removed once the rule names the new key.
    fn forget_old_keys(&self, io: &mut dyn Transport, current: &str) -> Result<()> {
        for key in self.pages(io, "deploy_keys")? {
            let title = key["title"].as_str().unwrap_or("");
            if title.starts_with("cfrg:remote_mirror_") && title != current {
                let id = key["id"].as_u64().ok_or("Missing deploy key id")?;
                expect(
                    call(
                        io,
                        &self.dest,
                        "DELETE",
                        format!("/projects/{}/deploy_keys/{id}", self.project()),
                        None,
                        false,
                    )?,
                    &[204],
                )?;
            }
        }
        Ok(())
    }

    fn project(&self) -> String {
        encode(&self.dest.path)
    }

    fn pages(&self, io: &mut dyn Transport, what: &str) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        for page in 1..=50 {
            let values = expect(
                call(
                    io,
                    &self.dest,
                    "GET",
                    format!(
                        "/projects/{}/{what}?per_page=100&page={page}",
                        self.project()
                    ),
                    None,
                    false,
                )?,
                &[200],
            )?;
            let values = values.as_array().ok_or("Malformed GitLab list")?.clone();
            let last = values.len() < 100;
            result.extend(values);
            if last {
                return Ok(result);
            }
        }
        Err("GitLab pagination bound exceeded".into())
    }
}

/// GitLab does not percent-decode the credentials of a mirror URL (verified
/// live: a `%3D` in a Bitbucket token fails authentication), so they go in raw.
/// Characters that would change the URL's structure cannot be sent that way.
fn userinfo(value: &str) -> Result<&str> {
    if value.is_empty()
        || value
            .bytes()
            .any(|b| !b.is_ascii_graphic() || b":@/?#%[]\\\"<>".contains(&b))
    {
        return Err("A mirror credential contains characters GitLab cannot take in a URL".into());
    }
    Ok(value)
}

/// A rule is locked when nobody but a key, a user or no one may push.
fn locked_form(rule: &Value) -> bool {
    let only_locked = |levels: &Value, push: bool| {
        levels.as_array().is_some_and(|rows| {
            !rows.is_empty()
                && rows.iter().all(|r| {
                    (r["access_level"] == 0
                        && r["user_id"].is_null()
                        && r["deploy_key_id"].is_null())
                        || (push && (!r["deploy_key_id"].is_null() || !r["user_id"].is_null()))
                })
        })
    };
    only_locked(&rule["push_access_levels"], true)
        && only_locked(&rule["merge_access_levels"], false)
}

impl Replica for Gitlab {
    fn site(&self) -> &Site {
        &self.site
    }
    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }
    fn verify(&self, io: &mut dyn Transport) -> Result<()> {
        crate::native::ensure_destination_repo(io, &self.repo, &self.dest, false, false)?
            .map(|_| ())
            .ok_or_else(|| "GitLab destination does not exist".into())
    }
    fn refs(&self, io: &mut dyn Transport) -> Result<Refs> {
        let mut refs = Refs::new();
        for (what, prefix) in [
            ("repository/branches", "refs/heads/"),
            ("repository/tags", "refs/tags/"),
        ] {
            for value in self.pages(io, what)? {
                let name = value["name"].as_str().ok_or("Missing ref name")?;
                let id = value["commit"]["id"].as_str().ok_or("Missing ref commit")?;
                refs.insert(format!("{prefix}{name}"), id.into());
            }
        }
        Ok(refs)
    }
    fn target(&self) -> Result<Target> {
        let host = self.dest.endpoint.origin.trim_start_matches("https://");
        Ok(Target {
            site: self.site.clone(),
            https: format!("{}/{}.git", self.dest.endpoint.origin, self.dest.path),
            key_url: self
                .dest
                .use_ssh
                .then(|| format!("ssh://git@{host}/{}.git", self.dest.path)),
            login: "oauth2".into(),
            secret_env: self.dest.password_env.clone(),
            principal: self.dest.mirror_user.clone(),
        })
    }
    fn mirrors(&self, io: &mut dyn Transport) -> Result<Vec<Mirror>> {
        let mut result = Vec::new();
        for value in self.pages(io, "remote_mirrors")? {
            let id = value["id"].as_u64().ok_or("Missing remote mirror id")?;
            result.push(Mirror {
                id: id.to_string(),
                address: value["url"]
                    .as_str()
                    .and_then(address_of)
                    .unwrap_or_default(),
                public_key: None,
                enabled: value["enabled"] == true,
                healthy: match value["update_status"].as_str() {
                    Some("finished") => Some(true),
                    Some("failed") => Some(false),
                    _ => None,
                },
            });
        }
        Ok(result)
    }
    fn add_mirror(&self, io: &mut dyn Transport, to: &Target) -> Result<Mirror> {
        // Credentials travel inside the URL (the API takes no separate fields)
        // and come back masked. The body leaves this process only through a
        // private temporary file handed to curl.
        let url = format!(
            "https://{}:{}@{}.git",
            userinfo(&to.login)?,
            userinfo(&secret(&to.secret_env)?)?,
            to.site.address
        );
        let value = expect(
            call(
                io,
                &self.dest,
                "POST",
                format!("/projects/{}/remote_mirrors", self.project()),
                Some(
                    json!({"url": url, "enabled": true, "auth_method": "password",
                    "keep_divergent_refs": false, "only_protected_branches": false}),
                ),
                false,
            )?,
            &[200, 201],
        )?;
        let id = value["id"].as_u64().ok_or("Missing created mirror id")?;
        if value["url"].as_str().and_then(address_of).as_deref() != Some(to.site.address.as_str()) {
            return Err(
                "Created mirror does not match the declared target; inspect before retry".into(),
            );
        }
        Ok(Mirror {
            id: id.to_string(),
            address: to.site.address.clone(),
            public_key: None,
            enabled: true,
            healthy: None,
        })
    }
    fn remove_mirror(&self, io: &mut dyn Transport, mirror: &Mirror) -> Result<()> {
        mirror
            .id
            .parse::<u64>()
            .map_err(|_| "Invalid GitLab mirror id")?;
        expect(
            call(
                io,
                &self.dest,
                "DELETE",
                format!("/projects/{}/remote_mirrors/{}", self.project(), mirror.id),
                None,
                false,
            )?,
            &[204],
        )?;
        Ok(())
    }
    fn run_mirror(&self, io: &mut dyn Transport, mirror: &Mirror) -> Result<()> {
        mirror
            .id
            .parse::<u64>()
            .map_err(|_| "Invalid GitLab mirror id")?;
        expect(
            call(
                io,
                &self.dest,
                "POST",
                format!(
                    "/projects/{}/remote_mirrors/{}/sync",
                    self.project(),
                    mirror.id
                ),
                None,
                false,
            )?,
            &[200, 204],
        )?;
        Ok(())
    }
    fn lock(&self, io: &mut dyn Transport, admit: &Admit) -> Result<()> {
        match admit {
            Admit::Nobody => crate::native::deny_branch_writes(io, &self.dest),
            Admit::User(id) if *id == self.dest.mirror_user => {
                crate::native::protect(io, &self.dest, &self.repo.default_branch, true).map(|_| ())
            }
            Admit::User(_) => Err("GitLab Free locks a receiver to its mirror key or to the declared mirror_user only".into()),
            Admit::Key { public_key, title } => {
                let mirror = json!({
                    "public_key": public_key,
                    "remote_name": title.strip_prefix("cfrg:").unwrap_or(title),
                });
                crate::native::protect_ssh(io, &self.dest, &self.repo.default_branch, &mirror, true)?;
                self.forget_old_keys(io, title)
            }
        }
    }
    fn unlock(&self, io: &mut dyn Transport) -> Result<()> {
        for rule in rules(io, &self.dest)? {
            let name = rule["name"]
                .as_str()
                .ok_or("Missing protected branch name")?;
            if name != "*" && !locked_form(&rule) {
                continue;
            }
            expect(
                call(
                    io,
                    &self.dest,
                    "DELETE",
                    format!(
                        "/projects/{}/protected_branches/{}",
                        self.project(),
                        encode(name)
                    ),
                    None,
                    false,
                )?,
                &[204],
            )?;
            if name != "*" {
                // Back to GitLab's own default for a protected branch.
                expect(
                    call(
                        io,
                        &self.dest,
                        "POST",
                        format!("/projects/{}/protected_branches", self.project()),
                        Some(
                            json!({"name": name, "push_access_level": 40, "merge_access_level": 40, "allow_force_push": false}),
                        ),
                        false,
                    )?,
                    &[201],
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfrg::native::http::{Request, Response};

    #[test]
    fn only_a_locked_rule_is_a_lock() {
        let lock = json!({"push_access_levels":[{"access_level":40,"deploy_key_id":9}],"merge_access_levels":[{"access_level":0}]});
        let deny = json!({"push_access_levels":[{"access_level":0}],"merge_access_levels":[{"access_level":0}]});
        let default = json!({"push_access_levels":[{"access_level":40}],"merge_access_levels":[{"access_level":40}]});
        assert!(locked_form(&lock));
        assert!(locked_form(&deny));
        assert!(!locked_form(&default));
    }

    struct Keys(Vec<String>);
    impl Transport for Keys {
        fn send(&mut self, request: Request) -> Result<Response> {
            let line = format!("{} {}", request.method, request.path);
            self.0.push(line.clone());
            Ok(if request.method == "GET" {
                let body = if line.contains("page=1") {
                    json!([
                        {"id": 1, "title": "cfrg:remote_mirror_old"},
                        {"id": 2, "title": "cfrg:remote_mirror_new"},
                        {"id": 3, "title": "someone's own key"}
                    ])
                } else {
                    json!([])
                };
                Response { status: 200, body }
            } else {
                Response {
                    status: 204,
                    body: Value::Null,
                }
            })
        }
    }

    #[test]
    fn only_orphaned_cfrg_keys_are_removed_when_the_new_key_is_enrolled() {
        let repo: Repository = serde_json::from_value(json!({"path":"team/repo","source_id":1,"private":true,"default_branch":"main","content":"native-git","destinations":[]})).unwrap();
        let dest: Destination = serde_json::from_value(json!({"provider":"gitlab","endpoint":{"origin":"https://gitlab.example","token_env":"TOKEN"},"path":"team/repo","namespace":"2","mirror_user":"9","password_env":"TOKEN","use_ssh":true,"interval_seconds":3600})).unwrap();
        let mut io = Keys(Vec::new());
        Gitlab::new(&repo, &dest)
            .forget_old_keys(&mut io, "cfrg:remote_mirror_new")
            .unwrap();
        let deletes: Vec<_> = io.0.iter().filter(|l| l.starts_with("DELETE")).collect();
        assert_eq!(
            deletes,
            ["DELETE /api/v4/projects/team%2Frepo/deploy_keys/1"]
        );
    }

    #[test]
    fn mirror_credentials_go_into_the_url_raw_and_structure_characters_are_refused() {
        assert_eq!(userinfo("ATATT3-x_y=Z").unwrap(), "ATATT3-x_y=Z");
        for bad in ["a:b", "a@b", "a/b", "a%3Db", "a b", ""] {
            assert!(userinfo(bad).is_err(), "{bad}");
        }
    }
}
