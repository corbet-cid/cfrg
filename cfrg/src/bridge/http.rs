//! Bounded HTTP transport for the contribution bridge.
//!
//! One curl executor for every forge call: explicit environment, credential
//! expansion inside curl, no redirects or ambient proxy, bounded sizes and
//! timeouts, one shared request budget and pacing. Adapters (`cglb`, `cfgj`)
//! resolve exact endpoints, payloads and response shapes through typed
//! capabilities; this transport only executes fully resolved requests. The
//! only mutations any caller can express are the typed mutating methods on
//! those capabilities (GitLab note post + MR close; Forgejo pull create +
//! pull close); there is no generic mutation path.
use crate::{failure, process::Runner, Environment, Result};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    time::{Duration, Instant},
};

/// HTTP verbs the bridge may use. Mutations exist only as typed capability
/// methods in the adapters, never as free-form requests from orchestration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeMethod {
    Get,
    Post,
    Put,
    Patch,
}

impl BridgeMethod {
    fn as_str(&self) -> &'static str {
        match self {
            BridgeMethod::Get => "GET",
            BridgeMethod::Post => "POST",
            BridgeMethod::Put => "PUT",
            BridgeMethod::Patch => "PATCH",
        }
    }

    fn expected_status(&self) -> &'static str {
        match self {
            BridgeMethod::Post => "201",
            _ => "200",
        }
    }
}

/// How one forge authenticates bridge requests. The scheme selects the
/// header; the credential itself always comes from `token_env`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeAuth {
    Token,
    PrivateToken,
}

/// One fully resolved bridge HTTP request.
#[derive(Debug, Clone)]
pub struct BridgeRequest {
    pub origin: String,
    pub token_env: &'static str,
    pub auth: BridgeAuth,
    pub method: BridgeMethod,
    pub path: String,
    pub body: Option<Value>,
}

/// Bounded bridge transport. Implementations execute fully resolved
/// requests; endpoint knowledge lives in the adapters. Production
/// execution additionally passes every request through the authorization
/// policies before any credential lookup (see [`HttpTransport`]).
pub trait BridgeTransport {
    fn execute(&mut self, request: &BridgeRequest) -> Result<Value>;
}

/// Narrow authorization policy for bridge HTTP, owned by the adapters: the
/// exact endpoint, origin and credential rules for one forge. The transport
/// invokes every policy before credential lookup and any network I/O, for
/// EVERY request; anything no policy allows fails closed. There is no
/// permissive default and no fallback.
pub trait MutationPolicy {
    fn authorize(&self, request: &BridgeRequest) -> Result<()>;
}

/// Fixed child-process alias for the selected bridge credential. The curl
/// `--variable` import and both `--expand-header` templates in
/// [`HttpTransport::execute`] must name exactly this alias; the
/// provider-specific `token_env` name is used only for the parent-side
/// lookup and must never reach the child.
const CHILD_TOKEN_ALIAS: &str = "CFRG_BRIDGE_TOKEN";

/// Build the sanitized child environment for one authorized request: the
/// allowlisted base plus the policy-selected credential under the single
/// fixed alias. The provider name, the other provider's token and ambient
/// tokens are never inserted.
fn child_environment(request: &BridgeRequest) -> Result<Environment> {
    let mut environment = environment();
    environment.insert(CHILD_TOKEN_ALIAS.into(), token(request.token_env)?.into());
    Ok(environment)
}

/// Shared path hygiene for policies: absolute paths only, no traversal,
/// empty segments, backslashes, whitespace or control characters. Adapters
/// add their exact endpoint shapes on top.
pub fn reject_path_tricks(path: &str) -> Result<()> {
    if path.is_empty()
        || !path.starts_with('/')
        || path.contains('\\')
        || path.split('/').skip(1).any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.chars().any(|c| c.is_whitespace() || c.is_control())
        })
    {
        return Err(failure(format!("Refusing unsafe bridge path: {path}")));
    }
    Ok(())
}

/// A child receives only its own credential, never the other forge's token or
/// ambient Git/curl configuration. Credentials are expanded inside curl.
pub fn environment() -> Environment {
    let mut result = Environment::new();
    for key in ["PATH", "TMPDIR", "SSL_CERT_FILE", "SSL_CERT_DIR"] {
        if let Some(value) = std::env::var_os(key) {
            result.insert(key.into(), value);
        }
    }
    result
}

pub fn token(name: &str) -> Result<String> {
    let value = std::env::var(name).map_err(|_| failure(format!("Set {name}")))?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(failure("Invalid credential format"));
    }
    Ok(value)
}

/// Paginated inventory with the bridge bounds: requests until an EMPTY page
/// (instances may cap page size), at most 20 pages, and repeated pages fail
/// closed so an older entry can never hide behind a stuck page.
pub fn fetch_pages(
    transport: &mut dyn BridgeTransport,
    origin: &str,
    token_env: &'static str,
    auth: BridgeAuth,
    path: &str,
    page_param: &str,
) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    let mut previous = Vec::new();
    let separator = if path.contains('?') { '&' } else { '?' };
    for page in 1..=20 {
        let value = transport.execute(&BridgeRequest {
            origin: origin.into(),
            token_env,
            auth,
            method: BridgeMethod::Get,
            path: format!("{path}{separator}{page_param}=50&page={page}"),
            body: None,
        })?;
        let values = value
            .as_array()
            .ok_or_else(|| failure("Expected a paginated array"))?;
        if values.is_empty() {
            return Ok(result);
        }
        // Instance limits can cap a requested page below 50. Only an empty
        // page proves completion; repeated pages must not hide an older PR.
        if values == &previous {
            return Err(failure("Forge repeated an inventory page"));
        }
        previous = values.clone();
        result.extend(values.iter().cloned());
    }
    Err(failure(
        "Bridge inventory exceeds 20 pages; refusing incomplete reconciliation",
    ))
}

pub struct HttpTransport {
    deadline: Instant,
    last_request: Option<Instant>,
    count: usize,
    policies: Vec<Box<dyn MutationPolicy>>,
}

impl HttpTransport {
    pub fn new(timeout: Duration, policies: Vec<Box<dyn MutationPolicy>>) -> Result<Self> {
        Ok(Self {
            deadline: Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| failure("Bridge deadline is out of range"))?,
            last_request: None,
            count: 0,
            policies,
        })
    }
}

impl BridgeTransport for HttpTransport {
    fn execute(&mut self, request: &BridgeRequest) -> Result<Value> {
        // Authorize before credential lookup and before any network I/O.
        // An empty policy set refuses everything: there is no default allow.
        if !self
            .policies
            .iter()
            .any(|policy| policy.authorize(request).is_ok())
        {
            return Err(failure("Bridge request outside authorized scope"));
        }
        if self.count >= 200 {
            return Err(failure("Bridge API request budget exhausted"));
        }
        if let Some(last) = self.last_request {
            std::thread::sleep(Duration::from_secs(1).saturating_sub(last.elapsed()));
        }
        self.count += 1;
        self.last_request = Some(Instant::now());
        let environment = child_environment(request)?;
        let runner = Runner::until(std::env::current_dir()?, environment, self.deadline)?
            .with_stderr_events()
            .without_child_stderr();
        let output = tempfile::NamedTempFile::new()?;
        let mut input = tempfile::NamedTempFile::new()?;
        let header = match request.auth {
            BridgeAuth::Token => "Authorization: token {{CFRG_BRIDGE_TOKEN}}",
            BridgeAuth::PrivateToken => "PRIVATE-TOKEN: {{CFRG_BRIDGE_TOKEN}}",
        };
        let mut args: Vec<String> = [
            "curl",
            "--disable",
            "--silent",
            "--globoff",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--max-filesize",
            "4194304",
            "--proto",
            "=https",
            "--variable",
            "%CFRG_BRIDGE_TOKEN",
            "--expand-header",
            header,
            "--header",
            "Accept: application/json",
            "--output",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        args.push(output.path().to_string_lossy().into_owned());
        args.extend([
            "--write-out".into(),
            "%{http_code}".into(),
            "--request".into(),
            request.method.as_str().into(),
            "--url".into(),
            format!("{}{}", request.origin, request.path),
        ]);
        if let Some(body) = &request.body {
            serde_json::to_writer(&mut input, body)?;
            input.flush()?;
            args.extend([
                "--header".into(),
                "Content-Type: application/json".into(),
                "--data-binary".into(),
                format!("@{}", input.path().display()),
            ]);
        }
        // No redirects, retry flags, response bodies in errors, or ambient proxy.
        let status = runner.run(&args, true)?;
        if status != request.method.expected_status() {
            return Err(failure(format!(
                "Bridge API {} failed with HTTP {status}; no automatic retry",
                request.method.as_str()
            )));
        }
        Ok(serde_json::from_slice(&fs::read(output.path())?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Allow;
    struct Deny;
    impl MutationPolicy for Allow {
        fn authorize(&self, _request: &BridgeRequest) -> Result<()> {
            Ok(())
        }
    }
    impl MutationPolicy for Deny {
        fn authorize(&self, _request: &BridgeRequest) -> Result<()> {
            Err(failure("denied"))
        }
    }

    fn get(token_env: &'static str) -> BridgeRequest {
        BridgeRequest {
            origin: "https://forge.example".into(),
            token_env,
            auth: BridgeAuth::Token,
            method: BridgeMethod::Get,
            path: "/api/v1/repos/team/project".into(),
            body: None,
        }
    }

    #[test]
    fn child_alias_carries_only_the_selected_credential() {
        // Unique variable names: parallel tests and ambient provider tokens
        // cannot collide; values are ASCII-graphic per token() rules.
        // The curl `--variable` import and both `--expand-header` templates
        // must name CHILD_TOKEN_ALIAS; this test pins the environment side
        // for both auth modes and both provider selectors, and proves the
        // provider name plus any other token never reach the child.
        for (auth, lookup) in [
            (BridgeAuth::Token, "CFRG_BRIDGE_TEST_TOKEN_A"),
            (BridgeAuth::PrivateToken, "CFRG_BRIDGE_TEST_TOKEN_B"),
        ] {
            std::env::set_var(lookup, "s3cr3t-test-value");
            std::env::set_var("CFRG_BRIDGE_TEST_TOKEN_DECOY", "decoy-value");
            let request = BridgeRequest {
                origin: "https://forge.example".into(),
                token_env: lookup,
                auth,
                method: BridgeMethod::Get,
                path: "/api/v1/repos/team/project".into(),
                body: None,
            };
            let environment = child_environment(&request).unwrap();
            assert_eq!(CHILD_TOKEN_ALIAS, "CFRG_BRIDGE_TOKEN");
            assert_eq!(
                environment.get(std::ffi::OsStr::new(CHILD_TOKEN_ALIAS)),
                Some(&std::ffi::OsString::from("s3cr3t-test-value"))
            );
            assert!(!environment.contains_key(std::ffi::OsStr::new(lookup)));
            assert!(!environment.contains_key(std::ffi::OsStr::new("CFRG_BRIDGE_TEST_TOKEN_DECOY")));
            std::env::remove_var(lookup);
            std::env::remove_var("CFRG_BRIDGE_TEST_TOKEN_DECOY");
        }
    }

    #[test]
    fn refusal_precedes_credential_lookup_and_network() {
        // The token variable does not exist, yet refusal (not a credential
        // error) must surface: the gate runs before credential lookup, and
        // no child process is spawned.
        let mut transport =
            HttpTransport::new(Duration::from_secs(5), vec![Box::new(Deny)]).unwrap();
        let error = transport
            .execute(&get("CFRG_BRIDGE_MISSING_TOKEN"))
            .unwrap_err();
        assert_eq!(error.to_string(), "Bridge request outside authorized scope");
    }

    #[test]
    fn empty_policies_refuse_everything_and_lookup_survives_allow() {
        let mut transport = HttpTransport::new(Duration::from_secs(5), Vec::new()).unwrap();
        assert!(transport.execute(&get("CFRG_BRIDGE_TOKEN")).is_err());
        let mut transport =
            HttpTransport::new(Duration::from_secs(5), vec![Box::new(Allow)]).unwrap();
        // Allowed through the gate, then refused at credential lookup
        // instead of spawning curl: lookup still enforced after the gate.
        let error = transport
            .execute(&get("CFRG_BRIDGE_MISSING_TOKEN"))
            .unwrap_err();
        assert_eq!(error.to_string(), "Set CFRG_BRIDGE_MISSING_TOKEN");
    }

    #[test]
    fn path_hygiene_rejects_tricks() {
        for bad in [
            "",
            "relative/path",
            "/api/../escape",
            "/api//double",
            "/api/./dot",
            "/api/back\\slash",
            "/api/with space",
            "/api/with\nnewline",
        ] {
            assert!(reject_path_tricks(bad).is_err(), "{bad:?}");
        }
        assert!(reject_path_tricks("/api/v4/projects/42").is_ok());
    }
}
