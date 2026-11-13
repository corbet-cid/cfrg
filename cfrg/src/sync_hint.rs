//! The tag pusher's hint to the forge bridge (operating model R45).
//!
//! A release reaches a copy through the bridge's release module, but only once its tag is on the copy. A tag that
//! arrives late (a held or paced push) would leave the release `pending_tag` until a run by hand, so every cfrg
//! pusher, right after a push that brought a tag to a copy, posts `{"repository": "OWNER/REPO", "tag": "TAG"}` to
//! the bridge's `/hooks/sync`, signed like Forgejo's own deliveries: the hex HMAC-SHA256 of the raw body in
//! `X-Forgejo-Signature`, with the Forgejo key of the bridge (`hmac_forgejo`). The request carries no authority:
//! the bridge reads everything else from Forgejo, so a repeated hint only repeats an idempotent sync.
//!
//! A hint that fails never fails the push that preceded it; the pusher reports it.
use crate::{failure, process::Runner, serve::hmac_sha256, Environment, Result};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};

/// Where the hint goes. Optional in each pusher's configuration: without it no hint is sent.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SyncHint {
    /// The bridge's internal route, `https://<bridge>/hooks/sync`.
    pub url: String,
    /// The environment variable that holds the HMAC key (the bridge's `hmac_forgejo`).
    pub key_env: String,
    /// CA bundle that verifies the bridge's certificate (the cluster's service CA); the system bundle when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<PathBuf>,
}

/// The exact body of one hint.
pub fn body(repository: &str, tag: &str) -> String {
    serde_json::json!({"repository": repository, "tag": tag}).to_string()
}

/// `X-Forgejo-Signature` of a body: lowercase hex HMAC-SHA256.
pub fn signature(key: &[u8], body: &str) -> String {
    hmac_sha256(key, body.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl SyncHint {
    pub fn validate(&self) -> Result<()> {
        if !self.url.starts_with("https://") || !self.url.ends_with("/hooks/sync") {
            return Err(failure(
                "sync_hint.url must be the bridge's https://…/hooks/sync",
            ));
        }
        if self.key_env.is_empty()
            || !self
                .key_env
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(failure(
                "sync_hint.key_env must name an environment variable",
            ));
        }
        Ok(())
    }

    /// Post one hint and return the HTTP status (202 = queued). The key is read from the environment here, so a
    /// missing key fails only the hint. The signature reaches curl through its environment, never its argv.
    pub fn send(&self, repository: &str, tag: &str) -> Result<u16> {
        self.validate()?;
        let key = std::env::var(&self.key_env)
            .map_err(|_| failure(format!("{} is not set", self.key_env)))?;
        let body = body(repository, tag);
        let mut environment: Environment = std::env::vars_os().collect();
        environment.insert(
            "CFRG_SYNC_HINT_SIGNATURE".into(),
            signature(key.as_bytes(), &body).into(),
        );
        let mut args: Vec<String> = [
            "curl",
            "--disable",
            "--silent",
            "--globoff",
            "--proto",
            "=https",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}",
            "--variable",
            "%CFRG_SYNC_HINT_SIGNATURE",
            "--expand-header",
            "X-Forgejo-Signature: {{CFRG_SYNC_HINT_SIGNATURE}}",
            "--header",
            "Content-Type: application/json",
        ]
        .map(String::from)
        .to_vec();
        if let Some(ca) = &self.ca_file {
            args.push("--cacert".into());
            args.push(ca.display().to_string());
        }
        args.extend(["--data-binary".into(), body, self.url.clone()]);
        let out = Runner::new(PathBuf::from("."), environment, Duration::from_secs(45))?
            .with_stderr_events()
            .without_child_stderr()
            .run(&args, true)?;
        out.trim()
            .parse()
            .map_err(|_| failure("the bridge answered without an HTTP status"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_and_its_signature_are_what_the_bridge_verifies() {
        let body = body("corbet-fsl/ccvl", "v0.1.0");
        assert_eq!(body, r#"{"repository":"corbet-fsl/ccvl","tag":"v0.1.0"}"#);
        // `printf %s "$body" | openssl dgst -sha256 -hmac k -hex`
        assert_eq!(
            signature(b"k", &body),
            "1d20e3008a8429315606c939d537a45152c61b4bbcb6b7fed6cf95c7143b05d4"
        );
        assert!(crate::serve::verify_signature(
            b"k",
            body.as_bytes(),
            &signature(b"k", &body)
        ));
    }

    #[test]
    fn only_an_https_bridge_route_and_a_plain_variable_are_accepted() {
        let ok = SyncHint {
            url: "https://forge-bridge.example:5443/hooks/sync".into(),
            key_env: "CFRG_SYNC_HINT_KEY".into(),
            ca_file: None,
        };
        assert!(ok.validate().is_ok());
        for bad in [
            SyncHint {
                url: "http://forge-bridge.example/hooks/sync".into(),
                ..ok.clone()
            },
            SyncHint {
                url: "https://forge-bridge.example/hooks/release".into(),
                ..ok.clone()
            },
            SyncHint {
                key_env: "key; rm".into(),
                ..ok.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
