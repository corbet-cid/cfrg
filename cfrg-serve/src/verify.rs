//! Signature verification, one function per forge, and the replay guard.
//!
//! Every verifier works on the RAW body and runs before anything is parsed. The comparison is constant time
//! (`Mac::verify_slice`). A verifier returns the delivery id the sender gave, which the replay guard remembers.
use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, Mac};
use http::HeaderMap;
use sha2::Sha256;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

type HmacSha256 = Hmac<Sha256>;

/// GitLab's Standard Webhooks timestamp tolerance, in seconds (either direction).
pub const TIMESTAMP_TOLERANCE: i64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    /// No key is configured for this forge: nothing can be verified.
    NoKey,
    /// The signature header (or a header the signature covers) is absent.
    Missing,
    /// A signature is present and does not match.
    Wrong,
    /// GitLab: the timestamp is outside the tolerance.
    Stale,
}

impl Reject {
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoKey => "no-key",
            Self::Missing => "missing",
            Self::Wrong => "wrong",
            Self::Stale => "stale",
        }
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn mac_of(key: &[u8]) -> HmacSha256 {
    // HMAC accepts a key of any length.
    HmacSha256::new_from_slice(key).unwrap_or_else(|_| unreachable!("HMAC takes any key length"))
}

fn hex_matches(key: &[u8], body: &[u8], value: &str) -> bool {
    let value = value.trim();
    let value = value.strip_prefix("sha256=").unwrap_or(value);
    let Ok(expected) = hex::decode(value) else {
        return false;
    };
    let mut mac = mac_of(key);
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Forgejo: hex HMAC-SHA256 of the body in `X-Forgejo-Signature` (also `X-Gitea-Signature`, and the
/// `X-Hub-Signature-256` form). Delivery id: `X-Forgejo-Delivery`.
pub fn forgejo(
    key: Option<&str>,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Option<String>, Reject> {
    let key = key.ok_or(Reject::NoKey)?;
    let mut seen = false;
    for name in [
        "x-forgejo-signature",
        "x-gitea-signature",
        "x-hub-signature-256",
    ] {
        if let Some(value) = header(headers, name) {
            seen = true;
            if hex_matches(key.as_bytes(), body, value) {
                return Ok(["x-forgejo-delivery", "x-gitea-delivery"]
                    .iter()
                    .find_map(|n| header(headers, n))
                    .map(str::to_owned));
            }
        }
    }
    Err(if seen { Reject::Wrong } else { Reject::Missing })
}

/// GitHub: `X-Hub-Signature-256: sha256=<hex>`. The legacy SHA-1 header is ignored. `X-GitHub-Delivery` names the
/// EVENT (a manual redelivery carries the same id and the same body).
pub fn github(
    key: Option<&str>,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Option<String>, Reject> {
    let key = key.ok_or(Reject::NoKey)?;
    let value = header(headers, "x-hub-signature-256").ok_or(Reject::Missing)?;
    if !value.trim().starts_with("sha256=") || !hex_matches(key.as_bytes(), body, value) {
        return Err(Reject::Wrong);
    }
    Ok(header(headers, "x-github-delivery").map(str::to_owned))
}

/// The key of a GitLab signing token: `whsec_<base64 of 32 bytes>`; the key is the DECODED bytes.
pub fn gitlab_key(token: &str) -> Option<Vec<u8>> {
    let encoded = token.trim();
    let encoded = encoded.strip_prefix("whsec_").unwrap_or(encoded);
    let key = STANDARD.decode(encoded).ok()?;
    (key.len() >= 16).then_some(key)
}

/// GitLab (Standard Webhooks): `webhook-signature: v1,<base64 HMAC-SHA256(key, "{id}.{timestamp}.{body}")>`, a
/// space-separated list. `webhook-id` is the delivery id (per hook).
pub fn gitlab(
    token: Option<&str>,
    headers: &HeaderMap,
    body: &[u8],
    now_unix: i64,
) -> Result<Option<String>, Reject> {
    let key = gitlab_key(token.ok_or(Reject::NoKey)?).ok_or(Reject::NoKey)?;
    let id = header(headers, "webhook-id").ok_or(Reject::Missing)?;
    let timestamp = header(headers, "webhook-timestamp").ok_or(Reject::Missing)?;
    let signatures = header(headers, "webhook-signature").ok_or(Reject::Missing)?;
    let at: i64 = timestamp.trim().parse().map_err(|_| Reject::Wrong)?;
    if (now_unix - at).abs() > TIMESTAMP_TOLERANCE {
        return Err(Reject::Stale);
    }
    let mut mac = mac_of(&key);
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    for candidate in signatures.split_whitespace() {
        let Some(encoded) = candidate.strip_prefix("v1,") else {
            continue;
        };
        if let Ok(expected) = STANDARD.decode(encoded) {
            if mac.clone().verify_slice(&expected).is_ok() {
                return Ok(Some(id.to_owned()));
            }
        }
    }
    Err(Reject::Wrong)
}

/// Delivery ids already accepted. A bounded, expiring map in memory: a replay after a restart is still harmless
/// (the job re-reads the object and every write is idempotent), the guard only keeps duplicates from starting jobs.
pub struct Replay {
    ttl: Duration,
    capacity: usize,
    seen: Mutex<HashMap<String, Instant>>,
}

impl Replay {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity: capacity.max(1),
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// `true` when the id is new (and is now remembered); `false` for a replay.
    pub fn reserve(&self, key: &str, now: Instant) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = seen.get(key) {
            if now.saturating_duration_since(*at) < self.ttl {
                return false;
            }
        }
        if seen.len() >= self.capacity {
            let ttl = self.ttl;
            seen.retain(|_, at| now.saturating_duration_since(*at) < ttl);
        }
        if seen.len() >= self.capacity {
            // Still full of live ids: forget the oldest one rather than refuse a delivery.
            if let Some(oldest) = seen
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(k, _)| k.clone())
            {
                seen.remove(&oldest);
            }
        }
        seen.insert(key.to_owned(), now);
        true
    }

    /// Forget an id whose job could not be started, so the sender's redelivery is accepted.
    pub fn release(&self, key: &str) {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn hex_sig(key: &str, body: &[u8]) -> String {
        let mut mac = mac_of(key.as_bytes());
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    fn headers(pairs: &[(&'static str, String)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    const BODY: &[u8] = br#"{"action":"opened"}"#;

    #[test]
    fn github_valid_wrong_missing() {
        let good = headers(&[
            (
                "x-hub-signature-256",
                format!("sha256={}", hex_sig("k", BODY)),
            ),
            ("x-github-delivery", "d-1".into()),
        ]);
        assert_eq!(github(Some("k"), &good, BODY), Ok(Some("d-1".into())));
        let wrong = headers(&[(
            "x-hub-signature-256",
            format!("sha256={}", hex_sig("other", BODY)),
        )]);
        assert_eq!(github(Some("k"), &wrong, BODY), Err(Reject::Wrong));
        // The body changed after signing.
        assert_eq!(github(Some("k"), &good, b"{}"), Err(Reject::Wrong));
        assert_eq!(
            github(Some("k"), &HeaderMap::new(), BODY),
            Err(Reject::Missing)
        );
        // The legacy SHA-1 header alone is not a signature, and a bare hex value without the prefix is refused.
        let legacy = headers(&[("x-hub-signature", "sha1=00".into())]);
        assert_eq!(github(Some("k"), &legacy, BODY), Err(Reject::Missing));
        let bare = headers(&[("x-hub-signature-256", hex_sig("k", BODY))]);
        assert_eq!(github(Some("k"), &bare, BODY), Err(Reject::Wrong));
        assert_eq!(github(None, &good, BODY), Err(Reject::NoKey));
    }

    #[test]
    fn forgejo_valid_wrong_missing() {
        let good = headers(&[
            ("x-forgejo-signature", hex_sig("k", BODY)),
            ("x-forgejo-delivery", "u-1".into()),
        ]);
        assert_eq!(forgejo(Some("k"), &good, BODY), Ok(Some("u-1".into())));
        let wrong = headers(&[("x-forgejo-signature", "0".repeat(64))]);
        assert_eq!(forgejo(Some("k"), &wrong, BODY), Err(Reject::Wrong));
        assert_eq!(
            forgejo(Some("k"), &HeaderMap::new(), BODY),
            Err(Reject::Missing)
        );
        let hub = headers(&[(
            "x-hub-signature-256",
            format!("sha256={}", hex_sig("k", BODY)),
        )]);
        assert_eq!(forgejo(Some("k"), &hub, BODY), Ok(None));
        // Another forge's key signs nothing here.
        assert_eq!(forgejo(None, &good, BODY), Err(Reject::NoKey));
    }

    fn gitlab_token() -> String {
        format!("whsec_{}", STANDARD.encode([7u8; 32]))
    }

    fn gitlab_headers(id: &str, ts: i64, body: &[u8]) -> HeaderMap {
        let mut mac = mac_of(&[7u8; 32]);
        mac.update(format!("{id}.{ts}.").as_bytes());
        mac.update(body);
        let sig = STANDARD.encode(mac.finalize().into_bytes());
        headers(&[
            ("webhook-id", id.into()),
            ("webhook-timestamp", ts.to_string()),
            ("webhook-signature", format!("v1,{sig}")),
        ])
    }

    #[test]
    fn gitlab_valid_wrong_missing_stale() {
        let token = gitlab_token();
        let now = 1_800_000_000;
        let good = gitlab_headers("msg_1", now, BODY);
        assert_eq!(
            gitlab(Some(&token), &good, BODY, now),
            Ok(Some("msg_1".into()))
        );
        // The key is the DECODED bytes: signing with the token string itself must not verify.
        let mut mac = mac_of(token.as_bytes());
        mac.update(format!("msg_1.{now}.").as_bytes());
        mac.update(BODY);
        let by_string = headers(&[
            ("webhook-id", "msg_1".into()),
            ("webhook-timestamp", now.to_string()),
            (
                "webhook-signature",
                format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes())),
            ),
        ]);
        assert_eq!(
            gitlab(Some(&token), &by_string, BODY, now),
            Err(Reject::Wrong)
        );
        assert_eq!(gitlab(Some(&token), &good, b"{}", now), Err(Reject::Wrong));
        let mut missing = good.clone();
        missing.remove("webhook-signature");
        assert_eq!(
            gitlab(Some(&token), &missing, BODY, now),
            Err(Reject::Missing)
        );
        assert_eq!(
            gitlab(Some(&token), &HeaderMap::new(), BODY, now),
            Err(Reject::Missing)
        );
        // Outside the tolerance in either direction.
        assert_eq!(
            gitlab(Some(&token), &good, BODY, now + 301),
            Err(Reject::Stale)
        );
        assert_eq!(
            gitlab(Some(&token), &good, BODY, now - 301),
            Err(Reject::Stale)
        );
        assert_eq!(
            gitlab(Some(&token), &good, BODY, now + 299),
            Ok(Some("msg_1".into()))
        );
        // A second signature in the list is enough (key rotation).
        let mut rotated = good.clone();
        let current = rotated
            .get("webhook-signature")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        rotated.insert(
            "webhook-signature",
            HeaderValue::from_str(&format!("v1,AAAA {current}")).unwrap(),
        );
        assert_eq!(
            gitlab(Some(&token), &rotated, BODY, now),
            Ok(Some("msg_1".into()))
        );
        assert_eq!(gitlab(None, &good, BODY, now), Err(Reject::NoKey));
        assert_eq!(
            gitlab(Some("not base64 !"), &good, BODY, now),
            Err(Reject::NoKey)
        );
    }

    #[test]
    fn replay_is_refused_until_it_expires_or_is_released() {
        let guard = Replay::new(Duration::from_secs(10), 3);
        let t0 = Instant::now();
        assert!(guard.reserve("a", t0));
        assert!(!guard.reserve("a", t0 + Duration::from_secs(5)));
        assert!(guard.reserve("a", t0 + Duration::from_secs(11)), "expired");
        guard.release("a");
        assert!(
            guard.reserve("a", t0 + Duration::from_secs(12)),
            "released after a failed dispatch"
        );
        // Bounded: the oldest id goes first when the map is full of live ids.
        assert!(guard.reserve("b", t0 + Duration::from_secs(12)));
        assert!(guard.reserve("c", t0 + Duration::from_secs(12)));
        assert!(guard.reserve("d", t0 + Duration::from_secs(13)));
        assert!(guard.seen.lock().unwrap().len() <= 3);
    }
}
