//! Forgejo webhook registration for `cfrg serve`, native: one organisation hook
//! covers every repository of the organisation, a repository hook covers a
//! single repository. The hook is identified by its URL; its secret is sent
//! only when the hook is created or changed and is never read back or logged.
//!
//! Verified on Forgejo 15.0.9: `ALLOWED_HOST_LIST` on corbet's instance is `*`,
//! so cluster and LAN targets are allowed.
use cfrg::{
    land::{Capability, Support},
    native::{
        http::{expect, Auth, Request, Transport},
        path, Endpoint,
    },
    serve::EVENTS,
    Result,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;

pub const SERVE: Capability = Capability {
    support: Support::NativeFill,
    note: "native: signed webhooks (push, pull_request, pull_request_sync), organisation and repository hooks; no status event and no delivery retry, so cfrg adds a periodic reconcile",
};

#[derive(Clone, Copy)]
pub enum Scope<'a> {
    Org(&'a str),
    Repo(&'a str),
}

impl Scope<'_> {
    fn base(&self) -> Result<String> {
        match self {
            Self::Org(org) => {
                cfrg::native::component(org)?;
                Ok(format!("/api/v1/orgs/{org}/hooks"))
            }
            Self::Repo(repository) => {
                path(repository, false)?;
                Ok(format!("/api/v1/repos/{repository}/hooks"))
            }
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Org(org) => format!("org:{org}"),
            Self::Repo(repository) => format!("repo:{repository}"),
        }
    }
}

/// Make sure exactly one hook delivers to `url` with the events cfrg serve
/// understands. Without `apply` this only reports. `force` rewrites the hook
/// (secret rotation) even when nothing else differs. `secret` is required to
/// create or change a hook.
pub fn ensure(
    io: &mut dyn Transport,
    endpoint: &Endpoint,
    scope: Scope<'_>,
    url: &str,
    secret: Option<&str>,
    apply: bool,
    force: bool,
) -> Result<Value> {
    let base = scope.base()?;
    let mut call = |method: &'static str, suffix: &str, body: Option<Value>| {
        io.send(Request {
            endpoint: endpoint.clone(),
            auth: Auth::Token,
            method,
            path: format!("{base}{suffix}"),
            body,
            scope: format!("{}/api", endpoint.origin),
            creation: false,
        })
    };
    let mut found: Option<Value> = None;
    for page in 1..=10 {
        let listing = expect(
            call("GET", &format!("?limit=50&page={page}"), None)?,
            &[200],
        )?;
        let hooks = listing.as_array().ok_or("Malformed hook list")?;
        if let Some(hook) = hooks.iter().find(|h| h["config"]["url"] == url) {
            found = Some(hook.clone());
            break;
        }
        if hooks.len() < 50 {
            break;
        }
    }
    let label = scope.label();
    let report = |state: &str| json!({"scope": label, "url": url, "state": state});
    let desired = json!({
        "type": "forgejo",
        "active": true,
        "events": EVENTS,
        "config": {"url": url, "content_type": "json", "secret": secret},
    });
    let Some(hook) = found else {
        if !apply {
            return Ok(report("create-planned"));
        }
        secret.ok_or("A webhook secret is required to create the hook")?;
        expect(call("POST", "", Some(desired))?, &[200, 201])?;
        return Ok(report("created"));
    };
    let have: BTreeSet<&str> = hook["events"]
        .as_array()
        .map(|events| events.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let want: BTreeSet<&str> = EVENTS.iter().copied().collect();
    // Forgejo expands `pull_request` into every pull request sub-event, so the
    // stored list is a superset of what was asked for.
    let current = hook["active"] == true
        && want.is_subset(&have)
        && hook["config"]["content_type"] == "json"
        && hook["type"] == "forgejo";
    if current && !force {
        return Ok(report("ok"));
    }
    if !apply {
        return Ok(report("update-planned"));
    }
    secret.ok_or("A webhook secret is required to change the hook")?;
    let id = hook["id"].as_u64().ok_or("Missing hook id")?;
    // Every edit carries the secret: whether an edit without it would clear it
    // is not something to find out in production.
    let mut body = desired;
    if let Some(object) = body.as_object_mut() {
        object.remove("type");
    }
    expect(call("PATCH", &format!("/{id}"), Some(body))?, &[200, 201])?;
    Ok(report("updated"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cfrg::native::http::Response;
    use std::collections::VecDeque;

    struct Script(VecDeque<(&'static str, u16, Value)>, Vec<Request>);
    impl Transport for Script {
        fn send(&mut self, request: Request) -> Result<Response> {
            let (method, status, body) = self.0.pop_front().expect("unexpected call");
            assert_eq!(request.method, method, "{}", request.path);
            self.1.push(request);
            Ok(Response { status, body })
        }
    }

    fn endpoint() -> Endpoint {
        Endpoint {
            origin: "https://forge.example".into(),
            token_env: "TOKEN".into(),
        }
    }

    const URL: &str = "http://cfrg-serve.ci.svc.cluster.local:8080/hook";

    fn existing(events: Value, active: bool) -> Value {
        json!([{"id": 9, "type": "forgejo", "active": active, "events": events,
            "config": {"url": URL, "content_type": "json"}}])
    }

    #[test]
    fn creates_with_the_secret_in_the_body_only_when_applying() {
        let mut plan = Script(VecDeque::from([("GET", 200, json!([]))]), Vec::new());
        let report = ensure(
            &mut plan,
            &endpoint(),
            Scope::Org("corbet-libs"),
            URL,
            Some("s"),
            false,
            false,
        )
        .unwrap();
        assert_eq!(report["state"], "create-planned");
        let mut io = Script(
            VecDeque::from([("GET", 200, json!([])), ("POST", 201, Value::Null)]),
            Vec::new(),
        );
        let report = ensure(
            &mut io,
            &endpoint(),
            Scope::Org("corbet-libs"),
            URL,
            Some("s3cret"),
            true,
            false,
        )
        .unwrap();
        assert_eq!(report["state"], "created");
        assert!(!report.to_string().contains("s3cret"));
        let post = &io.1[1];
        assert_eq!(post.path, "/api/v1/orgs/corbet-libs/hooks");
        let body = post.body.as_ref().unwrap();
        assert_eq!(body["config"]["secret"], "s3cret");
        assert_eq!(body["config"]["content_type"], "json");
        assert_eq!(
            body["events"],
            json!(["push", "pull_request", "pull_request_sync"])
        );
        let mut none = Script(VecDeque::from([("GET", 200, json!([]))]), Vec::new());
        assert!(ensure(
            &mut none,
            &endpoint(),
            Scope::Repo("o/r"),
            URL,
            None,
            true,
            false
        )
        .is_err());
    }

    #[test]
    fn a_matching_hook_is_left_alone_and_a_drifted_one_is_patched() {
        let right = existing(json!(["push", "pull_request", "pull_request_sync"]), true);
        let mut io = Script(VecDeque::from([("GET", 200, right.clone())]), Vec::new());
        assert_eq!(
            ensure(
                &mut io,
                &endpoint(),
                Scope::Repo("o/r"),
                URL,
                None,
                true,
                false
            )
            .unwrap()["state"],
            "ok"
        );
        let expanded = existing(
            json!([
                "push",
                "pull_request",
                "pull_request_comment",
                "pull_request_sync"
            ]),
            true,
        );
        let mut io = Script(VecDeque::from([("GET", 200, expanded)]), Vec::new());
        assert_eq!(
            ensure(
                &mut io,
                &endpoint(),
                Scope::Repo("o/r"),
                URL,
                None,
                true,
                false
            )
            .unwrap()["state"],
            "ok"
        );
        let mut forced = Script(
            VecDeque::from([("GET", 200, right), ("PATCH", 200, Value::Null)]),
            Vec::new(),
        );
        let report = ensure(
            &mut forced,
            &endpoint(),
            Scope::Repo("o/r"),
            URL,
            Some("new"),
            true,
            true,
        )
        .unwrap();
        assert_eq!(report["state"], "updated");
        assert_eq!(forced.1[1].path, "/api/v1/repos/o/r/hooks/9");
        let drift = existing(json!(["push"]), false);
        let mut io = Script(VecDeque::from([("GET", 200, drift.clone())]), Vec::new());
        assert_eq!(
            ensure(
                &mut io,
                &endpoint(),
                Scope::Repo("o/r"),
                URL,
                Some("s"),
                false,
                false
            )
            .unwrap()["state"],
            "update-planned"
        );
        let mut io = Script(
            VecDeque::from([("GET", 200, drift), ("PATCH", 200, Value::Null)]),
            Vec::new(),
        );
        let patch_body = {
            ensure(
                &mut io,
                &endpoint(),
                Scope::Repo("o/r"),
                URL,
                Some("s"),
                true,
                false,
            )
            .unwrap();
            io.1[1].body.clone().unwrap()
        };
        assert!(patch_body.get("type").is_none());
        assert_eq!(patch_body["active"], true);
        assert_eq!(patch_body["config"]["secret"], "s");
    }

    #[test]
    fn other_hooks_are_never_touched() {
        let other = json!([{"id": 3, "type": "forgejo", "active": true, "events": ["push"],
            "config": {"url": "https://crow.example/api/hook", "content_type": "json"}}]);
        let mut io = Script(
            VecDeque::from([("GET", 200, other), ("POST", 201, Value::Null)]),
            Vec::new(),
        );
        let report = ensure(
            &mut io,
            &endpoint(),
            Scope::Repo("o/r"),
            URL,
            Some("s"),
            true,
            false,
        )
        .unwrap();
        assert_eq!(report["state"], "created");
    }
}
