//! Forgejo owns the replication data plane; these are its typed API operations.
use cfrg::{
    native::{
        component,
        http::{expect, Auth, Request, Transport},
        path, Destination, Endpoint, Repository,
    },
    Result,
};
use serde_json::{json, Value};

pub struct Mirrors<'a> {
    pub endpoint: &'a Endpoint,
    pub repository: &'a Repository,
}

impl Mirrors<'_> {
    fn call(
        &self,
        io: &mut dyn Transport,
        method: &'static str,
        suffix: &str,
        body: Option<Value>,
        statuses: &[u16],
    ) -> Result<Value> {
        path(&self.repository.path, false)?;
        expect(
            io.send(Request {
                endpoint: self.endpoint.clone(),
                auth: Auth::Token,
                method,
                path: format!("/api/v1/repos/{}{}", self.repository.path, suffix),
                body,
                scope: format!("{}/api", self.endpoint.origin),
                creation: false,
            })?,
            statuses,
        )
    }
    pub fn verify_source(&self, io: &mut dyn Transport) -> Result<()> {
        let value = self.call(io, "GET", "", None, &[200])?;
        if value["id"].as_u64() != Some(self.repository.source_id)
            || value["full_name"] != self.repository.path
            || value["private"] != self.repository.private
            || value["default_branch"] != self.repository.default_branch
            || value["mirror"] != false
        {
            return Err(
                "Forgejo source identity, privacy, primary status or default branch mismatch"
                    .into(),
            );
        }
        Ok(())
    }
    pub fn list(&self, io: &mut dyn Transport) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        for page in 1..=20 {
            let value = self.call(
                io,
                "GET",
                &format!("/push_mirrors?limit=50&page={page}"),
                None,
                &[200],
            )?;
            let values = value.as_array().ok_or("Malformed Forgejo mirror list")?;
            if values.is_empty() {
                return Ok(result);
            }
            for value in values {
                let name = value["remote_name"]
                    .as_str()
                    .ok_or("Missing mirror remote name")?;
                component(name)?;
                if result.iter().any(|old: &Value| old["remote_name"] == name) {
                    return Err("Repeated mirror page".into());
                }
                result.push(value.clone());
            }
        }
        Err("Forgejo mirror pagination bound exceeded".into())
    }
    pub fn create(
        &self,
        io: &mut dyn Transport,
        destination: &Destination,
        remote_address: &str,
        password: &str,
    ) -> Result<Value> {
        let mut body = json!({
            "remote_address": remote_address, "remote_username": match destination.provider {
                cfrg::native::Provider::Gitlab => "oauth2",
                cfrg::native::Provider::Bitbucket => "x-bitbucket-api-token-auth",
            },
            "remote_password": password, "interval": format!("{}s", destination.interval_seconds),
            "sync_on_commit": true, "branch_filter": destination.branch_filter,
        });
        if destination.use_ssh {
            let fields = body.as_object_mut().ok_or("Invalid mirror body")?;
            fields.remove("remote_username");
            fields.remove("remote_password");
            fields.insert("use_ssh".into(), json!(true));
        }
        self.call(io, "POST", "/push_mirrors", Some(body), &[200, 201])
    }
    /// Forgejo's API enqueues ALL mirrors on this repository, not one remote.
    /// The reconciler must verify that every listed mirror is authorized first.
    pub fn sync_now(&self, io: &mut dyn Transport) -> Result<()> {
        self.call(io, "POST", "/push_mirrors-sync", None, &[200])?;
        Ok(())
    }
    pub fn delete(&self, io: &mut dyn Transport, remote_name: &str) -> Result<()> {
        component(remote_name)?;
        self.call(
            io,
            "DELETE",
            &format!("/push_mirrors/{remote_name}"),
            None,
            &[204],
        )?;
        Ok(())
    }
}

/// Status deliberately excludes remote error text and URLs (can contain secrets).
pub fn status(mirror: &Value) -> Value {
    json!({"remote_name":mirror["remote_name"], "last_update":mirror["last_update"],
        "has_error":mirror["last_error"].as_str().is_none_or(|s| !s.is_empty()),
        "interval":mirror["interval"], "sync_on_commit":mirror["sync_on_commit"],
        "branch_filter":mirror["branch_filter"]})
}

pub fn matches(mirror: &Value, destination: &Destination, url: &str) -> bool {
    address_matches(mirror, url)
        && destination.use_ssh == mirror["public_key"].as_str().is_some_and(|s| !s.is_empty())
        && mirror["sync_on_commit"] == true
        && mirror["branch_filter"].as_str().unwrap_or("") == destination.branch_filter
        && duration_seconds(mirror["interval"].as_str().unwrap_or(""))
            == Some(destination.interval_seconds)
}

pub fn address_matches(mirror: &Value, url: &str) -> bool {
    let address = mirror["remote_address"].as_str().unwrap_or("");
    if url.starts_with("ssh://git@") {
        // Forgejo sanitizes userinfo from the API's SSH remote address.
        return address == url || address == url.replacen("ssh://git@", "ssh://", 1);
    }
    // Forgejo may return a sanitized credential-bearing authority. Compare only
    // the declared HTTPS host/path, never log the provider's returned address.
    let clean = address.strip_prefix("https://").map(|s| {
        let (authority, path) = s.split_once('/').unwrap_or((s, ""));
        format!(
            "https://{}/{}",
            authority.rsplit('@').next().unwrap_or(""),
            path
        )
    });
    clean.as_deref() == Some(url)
}

fn duration_seconds(value: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut number = String::new();
    for c in value.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let factor = match c {
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
        total = total.checked_add(number.parse::<u64>().ok()?.checked_mul(factor)?)?;
        number.clear();
    }
    if number.is_empty() && !value.is_empty() {
        Some(total)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_never_reports_remote_errors_or_credentials() {
        let value = status(&json!({"remote_address":"https://user:secret@host/repo", "last_error":"secret", "remote_name":"remote_mirror_abc"})).to_string();
        assert!(!value.contains("secret"));
        assert!(!value.contains("remote_address"));
        assert_eq!(duration_seconds("1h0m0s"), Some(3600));
    }

    #[test]
    fn sanitized_ssh_userinfo_does_not_change_destination_identity() {
        let mirror = json!({"remote_address":"ssh://gitlab.example/team/project.git"});
        assert!(address_matches(
            &mirror,
            "ssh://git@gitlab.example/team/project.git"
        ));
        assert!(!address_matches(
            &mirror,
            "ssh://git@other.example/team/project.git"
        ));
        assert!(!address_matches(
            &mirror,
            "ssh://git@gitlab.example/team/other.git"
        ));
    }

    struct SshCreate;
    impl Transport for SshCreate {
        fn send(&mut self, request: Request) -> Result<cfrg::native::http::Response> {
            assert_eq!(request.path, "/api/v1/repos/team/project/push_mirrors");
            assert_eq!(request.method, "POST");
            let body = request.body.unwrap();
            assert_eq!(body["use_ssh"], true);
            assert_eq!(body["sync_on_commit"], true);
            assert_eq!(body["interval"], "3600s");
            assert!(body.get("remote_password").is_none());
            assert!(body.get("remote_username").is_none());
            Ok(cfrg::native::http::Response {
                status: 200,
                body: json!({"remote_name":"remote_mirror_test"}),
            })
        }
    }
    #[test]
    fn ssh_creation_passes_no_http_credentials_and_accepts_native_200() {
        let repo: Repository = serde_json::from_value(json!({"path":"team/project","source_id":1,"private":true,"default_branch":"main","content":"native-git","destinations":[]})).unwrap();
        let dest: Destination = serde_json::from_value(json!({"provider":"gitlab","endpoint":{"origin":"https://gitlab.example","token_env":"TOKEN"},"path":"team/project","namespace":"2","mirror_user":"9","password_env":"TOKEN","use_ssh":true,"interval_seconds":3600})).unwrap();
        let endpoint = Endpoint {
            origin: "https://forge.example".into(),
            token_env: "TOKEN".into(),
        };
        let value = Mirrors {
            endpoint: &endpoint,
            repository: &repo,
        }
        .create(
            &mut SshCreate,
            &dest,
            "ssh://git@gitlab.example/team/project.git",
            "must-not-leak",
        )
        .unwrap();
        assert_eq!(value["remote_name"], "remote_mirror_test");
    }
}
