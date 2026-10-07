//! Forgejo as one repository of a replica set: reads, native push mirrors as
//! a sender, branch protection with push and merge whitelists as a receiver.
//!
//! Verified live on Forgejo 15 (see `docs/switch.md`). A protected branch
//! refuses force-push and deletion for everyone, so a receiver here stops a
//! sender whose history was rewritten or whose branch was deleted; the mirror
//! shows the error and nothing is lost. A mirror authenticates as a USER here
//! because Forgejo's SSH port is not reachable from other forges.
use cfrg::{
    land::{Capability, Support},
    model::Forge,
    native::{
        encode,
        http::{expect, Auth, Request, Response, Transport},
        path, Endpoint, Receiver, Repository,
    },
    replicate::{address_of, secret, Admit, Capabilities, Mirror, Refs, Replica, Site, Target},
    Result,
};
use serde_json::{json, Value};

pub const CAPABILITIES: Capabilities = Capabilities {
    forge: Forge::Forgejo,
    push_mirror: Capability {
        support: Support::Native,
        note: "native: push mirror to any Git remote, immediate on commit plus an interval, HTTPS credentials or a Forgejo-generated SSH key, also carries the wiki; refs are pushed forced",
    },
    mirror_key: true,
    pull_mirror: Capability {
        support: Support::NativeFill,
        note: "native only when the repository is created (migrate); an existing repository can never become a pull mirror",
    },
    pull_mirror_converts_existing: false,
    receiver_lock: Capability {
        support: Support::NativeFill,
        note: "native: branch protection with push and merge whitelists; cfrg fills: it locks every rule, because the first matching rule wins; protected branches refuse force-push and deletion for everyone",
    },
    switch: Capability {
        support: Support::NativeFill,
        note: "native: push mirrors and branch protection; cfrg fills the order (freeze, hash check, disable, unlock, create, verify); a normal repository cannot become a pull mirror, so the receiver stays a protected normal repository",
    },
};

const RULE: &str = "**";
const INTERVAL_SECONDS: u64 = 3600;

pub struct Forgejo {
    site: Site,
    endpoint: Endpoint,
    repo: Repository,
    receiver: Option<Receiver>,
}

impl Forgejo {
    pub fn new(endpoint: &Endpoint, repo: &Repository, receiver: Option<&Receiver>) -> Self {
        let host = endpoint.origin.trim_start_matches("https://");
        Self {
            site: Site::new(
                Forge::Forgejo,
                &endpoint.origin,
                &endpoint.origin,
                host,
                &repo.path,
                Some(repo.source_id.to_string()),
            ),
            endpoint: endpoint.clone(),
            repo: repo.clone(),
            receiver: receiver.cloned(),
        }
    }

    fn call(
        &self,
        io: &mut dyn Transport,
        method: &'static str,
        suffix: &str,
        body: Option<Value>,
    ) -> Result<Response> {
        path(&self.repo.path, false)?;
        io.send(Request {
            endpoint: self.endpoint.clone(),
            auth: Auth::Token,
            method,
            path: format!("/api/v1/repos/{}{suffix}", self.repo.path),
            body,
            scope: format!("{}/api", self.endpoint.origin),
            creation: false,
        })
    }

    fn pages(&self, io: &mut dyn Transport, what: &str) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        for page in 1..=50 {
            let values = expect(
                self.call(io, "GET", &format!("/{what}?limit=50&page={page}"), None)?,
                &[200],
            )?;
            let values = values.as_array().ok_or("Malformed Forgejo list")?.clone();
            let empty = values.is_empty();
            result.extend(values);
            if empty {
                return Ok(result);
            }
        }
        Err("Forgejo pagination bound exceeded".into())
    }

    /// The protection list is not paginated: every page repeats it.
    fn protections(&self, io: &mut dyn Transport) -> Result<Vec<Value>> {
        let value = expect(self.call(io, "GET", "/branch_protections", None)?, &[200])?;
        Ok(value
            .as_array()
            .ok_or("Malformed Forgejo protections")?
            .clone())
    }

    fn rule_path(name: &str) -> String {
        format!("/branch_protections/{}", encode(name))
    }
}

/// The settings that make a rule a lock for `admit`.
fn lock_body(admit: &Admit) -> Result<Value> {
    let mut body = json!({
        "enable_merge_whitelist": true, "merge_whitelist_usernames": [], "merge_whitelist_teams": [],
        "enable_push_whitelist": false, "push_whitelist_usernames": [], "push_whitelist_teams": [],
        "push_whitelist_deploy_keys": false, "apply_to_admins": true,
    });
    match admit {
        Admit::Nobody => body["enable_push"] = json!(false),
        Admit::User(user) => {
            body["enable_push"] = json!(true);
            body["enable_push_whitelist"] = json!(true);
            body["push_whitelist_usernames"] = json!([user]);
        }
        Admit::Key { .. } => return Err(
            "Forgejo is reached over HTTPS only (no SSH port is public): a receiver admits a user"
                .into(),
        ),
    }
    Ok(body)
}

/// A rule in the shape `lock_body` produces.
fn locked(rule: &Value) -> bool {
    rule["apply_to_admins"] == true
        && rule["enable_merge_whitelist"] == true
        && rule["merge_whitelist_usernames"]
            .as_array()
            .is_some_and(Vec::is_empty)
        && (rule["enable_push"] == false || rule["enable_push_whitelist"] == true)
}

fn unlocked_body() -> Value {
    json!({
        "enable_push": true, "enable_push_whitelist": false, "push_whitelist_usernames": [],
        "push_whitelist_deploy_keys": false, "enable_merge_whitelist": false,
        "merge_whitelist_usernames": [], "apply_to_admins": false,
    })
}

impl Replica for Forgejo {
    fn site(&self) -> &Site {
        &self.site
    }
    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }
    fn verify(&self, io: &mut dyn Transport) -> Result<()> {
        let value = expect(self.call(io, "GET", "", None)?, &[200])?;
        if value["id"].as_u64() != Some(self.repo.source_id)
            || value["full_name"] != self.repo.path
            || value["private"] != self.repo.private
            || value["mirror"] != false
        {
            return Err("Forgejo identity, privacy or mirror status mismatch".into());
        }
        Ok(())
    }
    fn refs(&self, io: &mut dyn Transport) -> Result<Refs> {
        let mut refs = Refs::new();
        for value in self.pages(io, "branches")? {
            let name = value["name"].as_str().ok_or("Missing branch name")?;
            let id = value["commit"]["id"]
                .as_str()
                .ok_or("Missing branch commit")?;
            refs.insert(format!("refs/heads/{name}"), id.into());
        }
        for value in self.pages(io, "tags")? {
            let name = value["name"].as_str().ok_or("Missing tag name")?;
            let id = value["commit"]["sha"]
                .as_str()
                .ok_or("Missing tag commit")?;
            refs.insert(format!("refs/tags/{name}"), id.into());
        }
        Ok(refs)
    }
    fn target(&self) -> Result<Target> {
        let receiver = self
            .receiver
            .as_ref()
            .ok_or("The placement declares no `receiver` identity for Forgejo")?;
        Ok(Target {
            site: self.site.clone(),
            https: format!("{}/{}.git", self.endpoint.origin, self.repo.path),
            key_url: None,
            login: receiver.mirror_user.clone(),
            secret_env: receiver.password_env.clone(),
            principal: receiver.mirror_user.clone(),
        })
    }
    fn mirrors(&self, io: &mut dyn Transport) -> Result<Vec<Mirror>> {
        let mut result = Vec::new();
        for value in self.pages(io, "push_mirrors")? {
            let id = value["remote_name"]
                .as_str()
                .ok_or("Missing mirror remote name")?;
            cfrg::native::component(id)?;
            let error = value["last_error"].as_str().is_some_and(|e| !e.is_empty());
            result.push(Mirror {
                id: id.into(),
                address: value["remote_address"]
                    .as_str()
                    .and_then(address_of)
                    .unwrap_or_default(),
                public_key: value["public_key"]
                    .as_str()
                    .filter(|k| !k.is_empty())
                    .map(String::from),
                enabled: true,
                healthy: if error {
                    Some(false)
                } else {
                    value["last_update"].as_str().map(|_| true)
                },
            });
        }
        Ok(result)
    }
    fn add_mirror(&self, io: &mut dyn Transport, to: &Target) -> Result<Mirror> {
        let mut body = json!({
            "interval": format!("{INTERVAL_SECONDS}s"), "sync_on_commit": true, "branch_filter": "",
        });
        match &to.key_url {
            Some(url) => {
                body["remote_address"] = json!(url);
                body["use_ssh"] = json!(true);
            }
            None => {
                body["remote_address"] = json!(to.https);
                body["remote_username"] = json!(to.login);
                body["remote_password"] = json!(secret(&to.secret_env)?);
            }
        }
        let value = expect(
            self.call(io, "POST", "/push_mirrors", Some(body))?,
            &[200, 201],
        )?;
        let id = value["remote_name"]
            .as_str()
            .ok_or("Missing created mirror remote name")?;
        cfrg::native::component(id)?;
        if value["remote_address"]
            .as_str()
            .and_then(address_of)
            .as_deref()
            != Some(to.site.address.as_str())
        {
            return Err(
                "Created mirror does not match the declared target; inspect before retry".into(),
            );
        }
        Ok(Mirror {
            id: id.into(),
            address: to.site.address.clone(),
            public_key: value["public_key"]
                .as_str()
                .filter(|k| !k.is_empty())
                .map(String::from),
            enabled: true,
            healthy: None,
        })
    }
    fn remove_mirror(&self, io: &mut dyn Transport, mirror: &Mirror) -> Result<()> {
        cfrg::native::component(&mirror.id)?;
        expect(
            self.call(io, "DELETE", &format!("/push_mirrors/{}", mirror.id), None)?,
            &[204],
        )?;
        Ok(())
    }
    fn run_mirror(&self, io: &mut dyn Transport, _mirror: &Mirror) -> Result<()> {
        // Forgejo enqueues every mirror of the repository; a switch leaves one direction only.
        expect(self.call(io, "POST", "/push_mirrors-sync", None)?, &[200])?;
        Ok(())
    }
    fn lock(&self, io: &mut dyn Transport, admit: &Admit) -> Result<()> {
        let body = lock_body(admit)?;
        let rules = self.protections(io)?;
        let mut have_rule = false;
        // The first matching rule wins, so every rule becomes the lock.
        for rule in &rules {
            let name = rule["rule_name"].as_str().ok_or("Missing rule name")?;
            have_rule |= name == RULE;
            expect(
                self.call(io, "PATCH", &Self::rule_path(name), Some(body.clone()))?,
                &[200, 201],
            )?;
        }
        if !have_rule {
            let mut create = body;
            create["rule_name"] = json!(RULE);
            create["required_approvals"] = json!(0);
            expect(
                self.call(io, "POST", "/branch_protections", Some(create))?,
                &[200, 201],
            )?;
        }
        Ok(())
    }
    fn unlock(&self, io: &mut dyn Transport) -> Result<()> {
        for rule in self.protections(io)? {
            let name = rule["rule_name"].as_str().ok_or("Missing rule name")?;
            if name == RULE {
                expect(
                    self.call(io, "DELETE", &Self::rule_path(name), None)?,
                    &[204],
                )?;
            } else if locked(&rule) {
                expect(
                    self.call(io, "PATCH", &Self::rule_path(name), Some(unlocked_body()))?,
                    &[200, 201],
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_denies_merges_and_binds_admins_and_only_the_named_user_may_push() {
        let nobody = lock_body(&Admit::Nobody).unwrap();
        assert_eq!(nobody["enable_push"], false);
        assert_eq!(nobody["apply_to_admins"], true);
        let user = lock_body(&Admit::User("mirror".into())).unwrap();
        assert_eq!(user["push_whitelist_usernames"], json!(["mirror"]));
        assert_eq!(user["enable_merge_whitelist"], true);
        assert_eq!(user["merge_whitelist_usernames"], json!([]));
        assert!(lock_body(&Admit::Key {
            public_key: "k".into(),
            title: "t".into()
        })
        .is_err());
        // What the lock writes is recognised as a lock; a soft landing rule is not.
        assert!(locked(&nobody) && locked(&user));
        assert!(!locked(&unlocked_body()));
        assert!(!locked(
            &json!({"enable_push": true, "enable_push_whitelist": false})
        ));
    }
}
