//! Bounded, paced transport with durable rate windows and uncertain-write holds.
use super::{env_name, Endpoint};
use crate::{
    bridge::http::{environment, token},
    failure,
    process::Runner,
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy)]
pub enum Auth {
    Token,
    PrivateToken,
    Bearer,
}

/// Constructed only by adapters, never accepted as arbitrary CLI input.
pub struct Request {
    pub endpoint: Endpoint,
    pub auth: Auth,
    pub method: &'static str,
    pub path: String,
    pub body: Option<Value>,
    pub scope: String,
    pub creation: bool,
}
pub struct Response {
    pub status: u16,
    pub body: Value,
}
pub trait Transport {
    fn send(&mut self, request: Request) -> Result<Response>;
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    #[serde(default)]
    pub owned: BTreeMap<String, String>,
    pub windows: BTreeMap<String, Window>,
    pub pending: BTreeMap<String, String>,
    pub next_request: u64,
    pub next_creation: u64,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct Window {
    pub observed_at: u64,
    pub status: u16,
    /// None means operator intervention, not permission to retry.
    pub retry_at: Option<u64>,
}

pub struct Http {
    state_path: PathBuf,
    lock_path: PathBuf,
    pub state: State,
    count: u32,
    apply: bool,
}

pub fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

impl Http {
    pub fn open(path: &Path, apply: bool) -> Result<Self> {
        let lock_path = path.with_extension("lock");
        let mut lock = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .map_err(|_| {
                failure("Native state locked; inspect owner before recovering a stale lock")
            })?;
        writeln!(lock, "{}", std::process::id())?;
        let state = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => {
                fs::remove_file(&lock_path)?;
                return Err(e.into());
            }
        };
        match state {
            Ok(state) => Ok(Self {
                state_path: path.into(),
                lock_path,
                state,
                count: 0,
                apply,
            }),
            Err(e) => {
                fs::remove_file(&lock_path)?;
                Err(e.into())
            }
        }
    }
    pub fn save(&self) -> Result<()> {
        let parent = self
            .state_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut file, &self.state)?;
        file.as_file().sync_all()?;
        file.persist(&self.state_path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

impl Transport for Http {
    fn send(&mut self, request: Request) -> Result<Response> {
        request.endpoint.validate()?;
        env_name(&request.endpoint.token_env)?;
        crate::bridge::http::reject_path_tricks(&request.path)?;
        let write = request.method != "GET";
        if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&request.method)
            || (write && !self.apply)
        {
            return Err(failure("Native mutation requires apply mode"));
        }
        if self.count >= 10000 {
            return Err(failure("Native request budget exhausted"));
        }
        let timestamp = now()?;
        if let Some(window) = self.state.windows.get(&request.scope) {
            if (window.status != 402 || write)
                && window.retry_at.is_none_or(|until| timestamp < until)
            {
                return Err(failure(format!(
                    "Native rate/plan hold for {}; inspect recorded window",
                    request.scope
                )));
            }
        }
        if write && self.state.pending.contains_key(&request.scope) {
            return Err(failure("Uncertain prior write; read provider state and resolve the recorded intent before retrying"));
        }
        let deadline = self.state.next_request.max(if request.creation {
            self.state.next_creation
        } else {
            0
        });
        if deadline.saturating_sub(timestamp) > 120 {
            return Err(failure("Pacing deadline is invalid or not yet due"));
        }
        std::thread::sleep(Duration::from_secs(deadline.saturating_sub(timestamp)));
        let timestamp = now()?;
        self.state.next_request = timestamp + 12;
        if request.creation {
            self.state.next_creation = timestamp + 60;
        }
        if write {
            self.state.pending.insert(
                request.scope.clone(),
                format!("{} {} at {}", request.method, request.path, timestamp),
            );
        }
        self.save()?;
        self.count += 1;
        if write {
            eprintln!(
                "{}",
                serde_json::json!({"event":"native-intent","method":request.method,"path":request.path,"scope":request.scope})
            );
        }
        let mut env = environment();
        env.insert(
            "CFRG_NATIVE_TOKEN".into(),
            token(&request.endpoint.token_env)?.into(),
        );
        let runner = Runner::new(std::env::current_dir()?, env, Duration::from_secs(45))?
            .with_stderr_events()
            .without_child_stderr();
        let output = tempfile::NamedTempFile::new()?;
        let headers = tempfile::NamedTempFile::new()?;
        let mut input = tempfile::NamedTempFile::new()?;
        let header = match request.auth {
            Auth::Token => "Authorization: token {{CFRG_NATIVE_TOKEN}}",
            Auth::PrivateToken => "PRIVATE-TOKEN: {{CFRG_NATIVE_TOKEN}}",
            Auth::Bearer => "Authorization: Bearer {{CFRG_NATIVE_TOKEN}}",
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
            "%CFRG_NATIVE_TOKEN",
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
            "--dump-header".into(),
            headers.path().to_string_lossy().into_owned(),
            "--write-out".into(),
            "%{http_code}".into(),
            "--request".into(),
            request.method.into(),
            "--url".into(),
            format!("{}{}", request.endpoint.origin, request.path),
        ]);
        if let Some(body) = request.body {
            serde_json::to_writer(&mut input, &body)?;
            input.flush()?;
            args.extend([
                "--header".into(),
                "Content-Type: application/json".into(),
                "--data-binary".into(),
                format!("@{}", input.path().display()),
            ]);
        }
        // No redirects, ambient credentials/proxies, stderr, retries or response-body diagnostics.
        let status: u16 = runner
            .run(&args, true)?
            .trim()
            .parse()
            .map_err(|_| failure("Unknown HTTP outcome"))?;
        if status == 0 {
            return Err(failure("Unknown HTTP outcome"));
        }
        eprintln!(
            "{}",
            serde_json::json!({"event":"native-response","method":request.method,"path":request.path,"status":status,"scope":request.scope})
        );
        // A server error on a mutation may have occurred after it committed.
        if write && status < 500 {
            self.state.pending.remove(&request.scope);
        }
        if status == 429 || status == 402 {
            let observed_at = now()?;
            let retry_at = if status == 402 {
                None
            } else {
                retry_after(&fs::read_to_string(headers.path())?, observed_at)
            };
            self.state.windows.insert(
                request.scope,
                Window {
                    observed_at,
                    status,
                    retry_at,
                },
            );
        }
        self.save()?;
        if matches!(status, 401 | 403 | 429 | 402) || status >= 500 {
            return Err(failure(format!(
                "Native API HTTP {status}; stopped without retry; window/intent recorded"
            )));
        }
        let bytes = fs::read(output.path())?;
        // Never parse or echo error bodies (may contain credentials).
        let body = if (200..300).contains(&status) && !bytes.is_empty() {
            serde_json::from_slice(&bytes)
                .map_err(|_| failure("Malformed native API success response"))?
        } else {
            Value::Null
        };
        Ok(Response { status, body })
    }
}

pub fn expect(response: Response, statuses: &[u16]) -> Result<Value> {
    if !statuses.contains(&response.status) {
        return Err(failure(format!(
            "Native API HTTP {}; no retry",
            response.status
        )));
    }
    Ok(response.body)
}

fn retry_after(headers: &str, now: u64) -> Option<u64> {
    // HTTP-date or missing/invalid delays remain held until explicit review.
    headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .rfind(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .and_then(|delay| now.checked_add(delay))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_do_not_guess_missing_or_date_delays() {
        assert_eq!(retry_after("Retry-After: 86400\r\n", 100), Some(86500));
        assert_eq!(
            retry_after("Retry-After: Wed, 07 Oct 2026 05:54:02 GMT", 100),
            None
        );
        assert_eq!(retry_after("", 100), None);
    }
    #[test]
    fn exclusive_state_lock_and_pending_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut http = Http::open(&path, true).unwrap();
        assert!(Http::open(&path, true).is_err());
        http.state.pending.insert("create".into(), "unknown".into());
        http.save().unwrap();
        drop(http);
        assert_eq!(
            Http::open(&path, true).unwrap().state.pending["create"],
            "unknown"
        );
    }

    #[test]
    fn recorded_limit_and_unknown_write_refuse_before_credentials_or_network() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut http = Http::open(&path, true).unwrap();
        let request = || Request {
            endpoint: Endpoint {
                origin: "https://forge.invalid".into(),
                token_env: "UNSET_NATIVE_TEST_TOKEN".into(),
            },
            auth: Auth::Token,
            method: "POST",
            path: "/api/repo".into(),
            body: None,
            scope: "create".into(),
            creation: true,
        };
        http.state.windows.insert(
            "create".into(),
            Window {
                observed_at: now().unwrap(),
                status: 429,
                retry_at: None,
            },
        );
        assert!(http
            .send(request())
            .err()
            .unwrap()
            .to_string()
            .contains("rate/plan hold"));
        http.state.windows.clear();
        http.state
            .pending
            .insert("create".into(), "POST unknown".into());
        assert!(http
            .send(request())
            .err()
            .unwrap()
            .to_string()
            .contains("Uncertain prior write"));
    }
}
