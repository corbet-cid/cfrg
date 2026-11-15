//! The forge side of the derived placement (`cfrg::native::derive`): lists the repositories and organisations of the
//! source forge, probes `.gitattributes` of a repository that is about to get a row, and reports what was derived and
//! what is held. Reads only.
use super::creds::Creds;
use cfrg::{
    native::{
        derive::{resolve, routes_lfs, Outcome, Source},
        http::{expect, Auth, Request, Transport},
        Endpoint, Placement,
    },
    Result,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn get(
    io: &mut dyn Transport,
    endpoint: &Endpoint,
    path: String,
) -> Result<cfrg::native::http::Response> {
    io.send(Request {
        endpoint: endpoint.clone(),
        auth: Auth::Token,
        method: "GET",
        path,
        body: None,
        scope: format!("{}/api", endpoint.origin),
        creation: false,
    })
}

fn source_of(item: &Value) -> Result<Source> {
    Ok(Source {
        id: item["id"].as_u64().ok_or("Missing repository id")?,
        path: item["full_name"]
            .as_str()
            .ok_or("Missing repository name")?
            .to_string(),
        private: item["private"] == true,
        default_branch: item["default_branch"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        empty: item["empty"] == true,
        fork: item["fork"] == true,
        mirror: item["mirror"] == true,
    })
}

/// Every repository the token sees, or the one repository `only`.
pub fn list_sources(
    io: &mut dyn Transport,
    endpoint: &Endpoint,
    only: Option<&str>,
) -> Result<Vec<Source>> {
    if let Some(path) = only {
        cfrg::native::path(path, false)?;
        let response = get(io, endpoint, format!("/api/v1/repos/{path}"))?;
        if response.status == 404 {
            return Ok(Vec::new());
        }
        return Ok(vec![source_of(&expect(response, &[200])?)?]);
    }
    let mut all = Vec::new();
    let mut seen = BTreeSet::new();
    for page in 1..=100 {
        let body = expect(
            get(
                io,
                endpoint,
                format!("/api/v1/repos/search?limit=50&page={page}&sort=id&order=asc"),
            )?,
            &[200],
        )?;
        let data = body["data"]
            .as_array()
            .ok_or("Malformed Forgejo repository search")?;
        if data.is_empty() {
            return Ok(all);
        }
        for item in data {
            let source = source_of(item)?;
            if !seen.insert(source.id) {
                return Err("Repeated repository page".into());
            }
            all.push(source);
        }
    }
    Err("Forgejo repository pagination bound exceeded".into())
}

/// Lowercase logins of the forge's organisations.
pub fn list_organizations(io: &mut dyn Transport, endpoint: &Endpoint) -> Result<BTreeSet<String>> {
    let mut all = BTreeSet::new();
    for page in 1..=20 {
        let body = expect(
            get(io, endpoint, format!("/api/v1/orgs?limit=50&page={page}"))?,
            &[200],
        )?;
        let data = body
            .as_array()
            .ok_or("Malformed Forgejo organization listing")?;
        if data.is_empty() {
            return Ok(all);
        }
        for item in data {
            if let Some(name) = item["username"].as_str().or_else(|| item["name"].as_str()) {
                all.insert(name.to_ascii_lowercase());
            }
        }
    }
    Err("Forgejo organization pagination bound exceeded".into())
}

fn base64_text(value: &str) -> Option<String> {
    let mut bits = 0u32;
    let mut count = 0;
    let mut bytes = Vec::new();
    for c in value
        .bytes()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
    {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(v);
        count += 6;
        if count >= 8 {
            count -= 8;
            bytes.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    String::from_utf8(bytes).ok()
}

/// Does the default branch route files through Git LFS? A missing `.gitattributes` is `false`.
fn uses_lfs(io: &mut dyn Transport, endpoint: &Endpoint, source: &Source) -> Result<bool> {
    let response = get(
        io,
        endpoint,
        format!(
            "/api/v1/repos/{}/contents/.gitattributes?ref={}",
            source.path, source.default_branch
        ),
    )?;
    if response.status == 404 {
        return Ok(false);
    }
    let body = expect(response, &[200])?;
    Ok(body["content"]
        .as_str()
        .and_then(base64_text)
        .is_some_and(|text| routes_lfs(&text)))
}

/// Adds the derived rows to `placement` (a no-op without a `derive` policy). `only` limits the pass to one repository
/// (an event). Returns the report rows: one `derived` row and one `derive-hold` row per owner and provider.
pub fn apply(
    creds: &Creds,
    placement: &mut Placement,
    io: &mut dyn Transport,
    only: Option<&str>,
) -> Result<Vec<Value>> {
    if placement.derive.is_none() {
        return Ok(Vec::new());
    }
    let endpoint = placement.source.clone();
    let listing = list_sources(io, &endpoint, only)?;
    let organizations = list_organizations(io, &endpoint)?;
    let before = placement.repositories.len();
    let outcome: Outcome = resolve(placement, &listing, &organizations, &mut |s| {
        uses_lfs(io, &endpoint, s)
    })?;
    creds.credentialed(placement, before);
    let mut rows = vec![json!({"state": "derived", "count": outcome.derived.len(),
        "repositories": outcome.derived, "skipped": outcome.skipped})];
    for hold in &outcome.holds {
        eprintln!(
            "{}",
            json!({"event": "derive-hold", "owner": hold.owner, "provider": hold.provider,
            "reason": hold.reason, "repositories": hold.repositories.len()})
        );
        rows.push(
            json!({"state": "derive-hold", "owner": hold.owner, "provider": hold.provider,
            "reason": hold.reason, "repositories": hold.repositories}),
        );
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    use cfrg::native::http::Response;

    /// A forge with three repositories: a public and an LFS one of a mapped organisation, and a stray one.
    struct Forge {
        requests: Vec<String>,
    }

    impl Transport for Forge {
        fn send(&mut self, request: Request) -> Result<Response> {
            self.requests.push(request.path.clone());
            let ok = |body: Value| Ok(Response { status: 200, body });
            let (path, query) = request.path.split_once('?').unwrap_or((&request.path, ""));
            if path == "/api/v1/repos/search" {
                return ok(if query.contains("page=1") {
                    json!({"data": [
                        {"id": 1, "full_name": "corbet-infra/cnew", "private": false, "default_branch": "main"},
                        {"id": 2, "full_name": "corbet-infra/cbig", "private": true, "default_branch": "main"},
                        {"id": 3, "full_name": "corbet-other/z", "private": false, "default_branch": "main"}]})
                } else {
                    json!({"data": []})
                });
            }
            if path == "/api/v1/orgs" {
                return ok(if query.contains("page=1") {
                    json!([{"username": "corbet-infra"}, {"username": "corbet-other"}])
                } else {
                    json!([])
                });
            }
            if path == "/api/v1/repos/corbet-infra/cbig/contents/.gitattributes" {
                return ok(json!({"content": "KiBmaWx0ZXI9bGZzCg=="}));
            }
            assert!(path.ends_with("/contents/.gitattributes"), "{path}");
            Ok(Response {
                status: 404,
                body: Value::Null,
            })
        }
    }

    #[test]
    fn a_listing_becomes_derived_rows_and_one_hold_per_missing_mapping() {
        let template = |origin: &str, env: &str| {
            json!({"endpoint": {"origin": origin, "token_env": env}, "mirror_user": "1", "password_env": env,
                   "interval_seconds": 3600})
        };
        let document = json!({"schema": 1, "source": {"origin": "https://forge.example", "token_env": "F"},
            "repositories": [], "derive": {
                "gitlab": template("https://gitlab.com", "GL"), "bitbucket": template("https://api.bitbucket.org", "BB"),
                "owners": {"corbet-infra": {"gitlab": "7", "bitbucket": "CORE"}}}});
        let mut placement = Placement::from_document(document.to_string().as_bytes()).unwrap();
        let creds = Creds::new(None);
        let mut forge = Forge {
            requests: Vec::new(),
        };
        let rows = apply(&creds, &mut placement, &mut forge, None).unwrap();
        // The Bitbucket credential is not mounted here: its destinations are dropped like a declared one's.
        let found: Vec<_> = placement
            .repositories
            .iter()
            .map(|r| (r.path.as_str(), r.private, r.destinations.len()))
            .collect();
        assert_eq!(
            found,
            [
                ("corbet-infra/cnew", false, 1),
                ("corbet-infra/cbig", true, 1)
            ]
        );
        assert_eq!(
            placement.repositories[1].content,
            cfrg::native::Content::Lfs
        );
        assert_eq!(rows[0]["count"], 2);
        let holds: Vec<_> = rows[1..]
            .iter()
            .map(|r| (r["owner"].clone(), r["provider"].clone()))
            .collect();
        assert_eq!(
            holds,
            [
                (json!("corbet-other"), json!("gitlab")),
                (json!("corbet-other"), json!("bitbucket"))
            ]
        );
        // The probe ran for the two repositories that got a row only.
        assert_eq!(
            forge
                .requests
                .iter()
                .filter(|p| p.contains("/contents/"))
                .count(),
            2
        );
    }

    #[test]
    fn base64_is_decoded_with_line_breaks_and_padding() {
        assert_eq!(base64_text("Kg==\n").as_deref(), Some("*"));
        assert_eq!(
            base64_text("KiBmaWx0ZXI9bGZz\nCg==").as_deref(),
            Some("* filter=lfs\n")
        );
        assert_eq!(base64_text("!!"), None);
    }
}
