//! Decision matrix: pinned/branch x primary identity x presence, ordering,
//! scope, conflicts, validation. Fake prober, no network or filesystem.
use clmr::{Outcome, Prober, RefKind, Repository, Request, Routing, Store, StoreKind};
use std::collections::BTreeMap;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const SHA2: &str = "fedcba9876543210fedcba9876543210fedcba98";

struct Table {
    answers: BTreeMap<(usize, String, String), Outcome>,
    calls: std::cell::RefCell<Vec<(usize, String, String)>>,
}

impl Table {
    fn new(answers: Vec<((usize, &str, &str), Outcome)>) -> Self {
        Self {
            answers: answers
                .into_iter()
                .map(|((s, p, n), o)| ((s, p.to_owned(), n.to_owned()), o))
                .collect(),
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl Prober for Table {
    fn probe(&self, store: usize, path: &str, need: &RefKind) -> Outcome {
        let key = match need {
            RefKind::Pinned(sha) => format!("pinned:{sha}"),
            RefKind::Moving(gitref) => format!("moving:{gitref}"),
        };
        self.calls
            .borrow_mut()
            .push((store, path.to_owned(), key.clone()));
        self.answers
            .get(&(store, path.to_owned(), key))
            .copied()
            .unwrap_or(Outcome::Miss)
    }
}

fn forgejo_store(identity: &str, scope: Vec<&str>) -> Store {
    Store {
        kind: StoreKind::HttpForge,
        location: "https://forge.example:3001".into(),
        identity: identity.into(),
        scope: scope.into_iter().map(str::to_owned).collect(),
        provider: Some("forgejo".into()),
        credential_env: Some("CFRG_RESOLVER_FORGEJO_TOKEN".into()),
        username: Some("ci".into()),
        trusted_single_user: false,
    }
}

fn repo(id: &str, path: &str, need: RefKind, primary: Option<&str>) -> Repository {
    Repository {
        id: id.into(),
        path: path.into(),
        r#ref: need,
        primary: primary.map(str::to_owned),
        primary_url: None,
        source_urls: vec![
            format!("https://pointer.example/{path}"),
            format!("https://pointer.example/{path}.git"),
        ],
    }
}

fn aliased_repo(id: &str, path: &str, need: RefKind, primary: Option<&str>) -> Repository {
    let mut repo = repo(id, path, need, primary);
    repo.source_urls.push(format!("https://github.com/{path}"));
    repo.source_urls
        .push(format!("https://github.com/{path}.git"));
    repo
}

fn aliases() -> Vec<clmr::Alias> {
    vec![clmr::Alias {
        url_prefix: "https://github.com/acme".into(),
        canonical_owner: "acme".into(),
    }]
}

fn request(repos: Vec<Repository>, stores: Vec<Store>) -> Request {
    Request {
        schema: 1,
        canonical_base: "https://pointer.example".into(),
        aliases: vec![],
        repositories: repos,
        stores,
        timeout_secs: 30,
        primary_source: None,
    }
}

fn pinned() -> RefKind {
    RefKind::Pinned(SHA.into())
}

fn branch() -> RefKind {
    RefKind::Moving("refs/heads/main".into())
}

#[test]
fn pinned_routes_first_verifying_store_regardless_of_primary() {
    let req = request(
        vec![repo("a", "acme/widget", pinned(), Some("forgejo"))],
        vec![forgejo_store("forgejo", vec!["acme"])],
    );
    let table = Table::new(vec![(
        (
            0,
            "acme/widget",
            "pinned:0123456789abcdef0123456789abcdef01234567",
        ),
        Outcome::Hit,
    )]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::Routed);
    assert_eq!(response.decisions[0].store, Some(0));
    assert_eq!(
        response.decisions[0].via.as_deref(),
        Some("https://forge.example:3001/acme/widget.git")
    );
    assert!(!response.decisions[0].instead_of.is_empty());
}

#[test]
fn pinned_prefers_first_hit_in_store_order() {
    let mut second = forgejo_store("forgejo", vec!["acme"]);
    second.location = "https://forge2.example:3001".into();
    let req = request(
        vec![repo("a", "acme/widget", pinned(), Some("forgejo"))],
        vec![forgejo_store("forgejo", vec!["acme"]), second],
    );
    let key = "pinned:0123456789abcdef0123456789abcdef01234567";
    let table = Table::new(vec![
        ((0, "acme/widget", key), Outcome::Hit),
        ((1, "acme/widget", key), Outcome::Hit),
    ]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].store, Some(0));
}

#[test]
fn pinned_skips_miss_and_denied_for_later_hit() {
    let mut second = forgejo_store("forgejo", vec!["acme"]);
    second.location = "https://forge2.example:3001".into();
    let req = request(
        vec![repo("a", "acme/widget", pinned(), Some("forgejo"))],
        vec![forgejo_store("forgejo", vec!["acme"]), second],
    );
    let key = "pinned:0123456789abcdef0123456789abcdef01234567";
    let table = Table::new(vec![
        ((0, "acme/widget", key), Outcome::Denied),
        ((1, "acme/widget", key), Outcome::Hit),
    ]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].store, Some(1));
}

#[test]
fn pinned_all_miss_or_errors_yield_pointer() {
    for outcome in [Outcome::Miss, Outcome::Denied, Outcome::Error] {
        let req = request(
            vec![repo("a", "acme/widget", pinned(), Some("forgejo"))],
            vec![forgejo_store("forgejo", vec!["acme"])],
        );
        let table = Table::new(vec![(
            (
                0,
                "acme/widget",
                "pinned:0123456789abcdef0123456789abcdef01234567",
            ),
            outcome,
        )]);
        let response = clmr::select(&req, &table).unwrap();
        assert_eq!(response.decisions[0].outcome, Routing::CanonicalPointer);
        assert!(response.decisions[0].via.is_none());
        // Pointer pairs stay under the canonical base (identity guard plus
        // `.git` normalization), never the store.
        for (from, to) in &response.decisions[0].instead_of {
            assert!(to.starts_with("https://pointer.example/"), "{from} -> {to}");
        }
    }
}

#[test]
fn pinned_ignores_unknown_primary_and_non_forgejo_primary() {
    // Unknown primary: pinned still routes on verified hash.
    let req = request(
        vec![repo("a", "acme/widget", pinned(), None)],
        vec![forgejo_store("forgejo", vec!["acme"])],
    );
    let table = Table::new(vec![(
        (
            0,
            "acme/widget",
            "pinned:0123456789abcdef0123456789abcdef01234567",
        ),
        Outcome::Hit,
    )]);
    assert_eq!(
        clmr::select(&req, &table).unwrap().decisions[0].outcome,
        Routing::Routed
    );
    // Non-Forgejo primary with Forgejo secondary present: pinned allowed.
    let req = request(
        vec![repo("a", "acme/widget", pinned(), Some("github"))],
        vec![forgejo_store("forgejo", vec!["acme"])],
    );
    let table = Table::new(vec![(
        (
            0,
            "acme/widget",
            "pinned:0123456789abcdef0123456789abcdef01234567",
        ),
        Outcome::Hit,
    )]);
    assert_eq!(
        clmr::select(&req, &table).unwrap().decisions[0].outcome,
        Routing::Routed
    );
}

#[test]
fn pinned_zero_stores_and_out_of_scope_yield_pointer_without_probes() {
    let req = request(
        vec![repo("a", "acme/widget", pinned(), Some("forgejo"))],
        vec![],
    );
    let table = Table::new(vec![]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::CanonicalPointer);
    // Canonical-only inventory: the bare self-map (identity guard) plus the
    // `.git` normalization, both targeting the pointer.
    assert_eq!(
        response.decisions[0].instead_of,
        vec![
            (
                "https://pointer.example/acme/widget".to_owned(),
                "https://pointer.example/acme/widget".to_owned()
            ),
            (
                "https://pointer.example/acme/widget.git".to_owned(),
                "https://pointer.example/acme/widget".to_owned()
            ),
        ]
    );

    let req = request(
        vec![repo("a", "acme/widget", pinned(), Some("forgejo"))],
        vec![forgejo_store("forgejo", vec!["other"])],
    );
    let table = Table::new(vec![]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::CanonicalPointer);
    assert!(table.calls.borrow().is_empty());
}

#[test]
fn branch_routes_only_primary_identity_store() {
    let req = request(
        vec![repo("a", "acme/widget", branch(), Some("forgejo"))],
        vec![forgejo_store("forgejo", vec!["acme"])],
    );
    let table = Table::new(vec![(
        (0, "acme/widget", "moving:refs/heads/main"),
        Outcome::Hit,
    )]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::Routed);
    assert_eq!(response.decisions[0].store, Some(0));
}

#[test]
fn branch_miss_denied_or_error_yields_pointer() {
    for outcome in [Outcome::Miss, Outcome::Denied, Outcome::Error] {
        let req = request(
            vec![repo("a", "acme/widget", branch(), Some("forgejo"))],
            vec![forgejo_store("forgejo", vec!["acme"])],
        );
        let table = Table::new(vec![(
            (0, "acme/widget", "moving:refs/heads/main"),
            outcome,
        )]);
        let response = clmr::select(&req, &table).unwrap();
        assert_eq!(response.decisions[0].outcome, Routing::CanonicalPointer);
    }
}

#[test]
fn branch_unknown_primary_or_identity_mismatch_never_probes() {
    // Unknown primary: pointer, no probe at all.
    let req = request(
        vec![repo("a", "acme/widget", branch(), None)],
        vec![forgejo_store("forgejo", vec!["acme"])],
    );
    let table = Table::new(vec![(
        (0, "acme/widget", "moving:refs/heads/main"),
        Outcome::Hit,
    )]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::CanonicalPointer);
    assert!(table.calls.borrow().is_empty());

    // Non-Forgejo primary with Forgejo secondary that HAS the ref: forbidden.
    let req = request(
        vec![repo("a", "acme/widget", branch(), Some("github"))],
        vec![forgejo_store("forgejo", vec!["acme"])],
    );
    let table = Table::new(vec![(
        (0, "acme/widget", "moving:refs/heads/main"),
        Outcome::Hit,
    )]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::CanonicalPointer);
    assert!(table.calls.borrow().is_empty());

    // Primary identity present but miss: pointer, secondary never consulted.
    let stores = vec![
        {
            let mut s = forgejo_store("github", vec!["acme"]);
            s.location = "https://github-proxy.example".into();
            s
        },
        forgejo_store("forgejo", vec!["acme"]),
    ];
    let req = request(
        vec![repo("a", "acme/widget", branch(), Some("github"))],
        stores,
    );
    let table = Table::new(vec![
        ((0, "acme/widget", "moving:refs/heads/main"), Outcome::Miss),
        ((1, "acme/widget", "moving:refs/heads/main"), Outcome::Hit),
    ]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::CanonicalPointer);
    assert!(table.calls.borrow().iter().all(|(s, _, _)| *s == 0));
}

#[test]
fn branch_continues_across_same_identity_stores_only() {
    // First primary-identity store misses, second hits, wrong-identity
    // store would also hit but must never be probed: first eligible hit wins.
    let same2 = {
        let mut s = forgejo_store("forgejo", vec!["acme"]);
        s.location = "https://forge2.example:3001".into();
        s
    };
    let other = {
        let mut s = forgejo_store("github", vec!["acme"]);
        s.location = "https://github-proxy.example".into();
        s
    };
    let req = request(
        vec![repo("a", "acme/widget", branch(), Some("forgejo"))],
        vec![forgejo_store("forgejo", vec!["acme"]), same2, other],
    );
    let key = "moving:refs/heads/main";
    let table = Table::new(vec![
        ((0, "acme/widget", key), Outcome::Miss),
        ((1, "acme/widget", key), Outcome::Hit),
        ((2, "acme/widget", key), Outcome::Hit),
    ]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::Routed);
    assert_eq!(response.decisions[0].store, Some(1));
    assert_eq!(
        response.decisions[0].via.as_deref(),
        Some("https://forge2.example:3001/acme/widget.git")
    );
    let probed: Vec<usize> = table.calls.borrow().iter().map(|(s, _, _)| *s).collect();
    assert_eq!(probed, vec![0, 1]);

    // Denied then unsupported then hit still continues; all-miss falls back.
    let table = Table::new(vec![
        ((0, "acme/widget", key), Outcome::Denied),
        ((1, "acme/widget", key), Outcome::Unsupported),
    ]);
    let req = request(
        vec![repo("a", "acme/widget", branch(), Some("forgejo"))],
        vec![forgejo_store("forgejo", vec!["acme"]), {
            let mut s = forgejo_store("forgejo", vec!["acme"]);
            s.location = "https://forge2.example:3001".into();
            s.provider = Some("gitea".into());
            s
        }],
    );
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::CanonicalPointer);
    assert!(response.decisions[0].note.contains("unsupported provider"));
}

#[test]
fn branch_zero_stores_yields_pointer() {
    let req = request(
        vec![repo("a", "acme/widget", branch(), Some("forgejo"))],
        vec![],
    );
    let table = Table::new(vec![]);
    assert_eq!(
        clmr::select(&req, &table).unwrap().decisions[0].outcome,
        Routing::CanonicalPointer
    );
}

#[test]
fn pinned_and_moving_to_different_destinations_fail_closed() {
    // Pinned routes store 0, moving (other primary) falls to pointer.
    let req = request(
        vec![
            repo("p", "acme/widget", pinned(), Some("forgejo")),
            repo("m", "acme/widget", branch(), Some("github")),
        ],
        vec![forgejo_store("forgejo", vec!["acme"])],
    );
    let table = Table::new(vec![
        (
            (
                0,
                "acme/widget",
                "pinned:0123456789abcdef0123456789abcdef01234567",
            ),
            Outcome::Hit,
        ),
        ((0, "acme/widget", "moving:refs/heads/main"), Outcome::Hit),
    ]);
    assert!(clmr::select(&req, &table).is_err());

    // Pinned store 0 vs moving primary store 1: also conflict.
    let mut second = forgejo_store("github", vec!["acme"]);
    second.location = "https://github-proxy.example".into();
    let req = request(
        vec![
            repo("p", "acme/widget", pinned(), Some("forgejo")),
            repo("m", "acme/widget", branch(), Some("github")),
        ],
        vec![forgejo_store("forgejo", vec!["acme"]), second],
    );
    let table = Table::new(vec![
        (
            (
                0,
                "acme/widget",
                "pinned:0123456789abcdef0123456789abcdef01234567",
            ),
            Outcome::Hit,
        ),
        ((1, "acme/widget", "moving:refs/heads/main"), Outcome::Hit),
    ]);
    assert!(clmr::select(&req, &table).is_err());
}

#[test]
fn same_destination_twice_and_double_pointer_are_consistent() {
    // Both route store 0: fine.
    let req = request(
        vec![
            repo("p", "acme/widget", pinned(), Some("forgejo")),
            repo("m", "acme/widget", branch(), Some("forgejo")),
        ],
        vec![forgejo_store("forgejo", vec!["acme"])],
    );
    let table = Table::new(vec![
        (
            (
                0,
                "acme/widget",
                "pinned:0123456789abcdef0123456789abcdef01234567",
            ),
            Outcome::Hit,
        ),
        ((0, "acme/widget", "moving:refs/heads/main"), Outcome::Hit),
    ]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions.len(), 2);
    assert!(response
        .decisions
        .iter()
        .all(|d| d.outcome == Routing::Routed));

    // Both pointer: fine.
    let req = request(
        vec![
            repo("p", "acme/widget", pinned(), Some("forgejo")),
            repo("m", "acme/widget", branch(), None),
        ],
        vec![forgejo_store("forgejo", vec!["acme"])],
    );
    let table = Table::new(vec![]);
    let response = clmr::select(&req, &table).unwrap();
    assert!(response
        .decisions
        .iter()
        .all(|d| d.outcome == Routing::CanonicalPointer));
}

#[test]
fn unsupported_provider_skips_with_note_never_guesses() {
    let mut unknown = forgejo_store("forgejo", vec!["acme"]);
    unknown.provider = Some("gitea".into());
    let req = request(
        vec![repo("a", "acme/widget", pinned(), Some("forgejo"))],
        vec![unknown],
    );
    let table = Table::new(vec![(
        (
            0,
            "acme/widget",
            "pinned:0123456789abcdef0123456789abcdef01234567",
        ),
        Outcome::Unsupported,
    )]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::CanonicalPointer);
    assert!(response.decisions[0].note.contains("unsupported provider"));
}

#[test]
fn filesystem_store_routes_file_url_without_credential_scope() {
    let store = Store {
        kind: StoreKind::Filesystem,
        location: "/srv/repos".into(),
        identity: "forgejo".into(),
        scope: vec!["acme/widget".into()],
        provider: None,
        credential_env: None,
        username: None,
        trusted_single_user: true,
    };
    let req = request(
        vec![repo("a", "acme/widget", pinned(), Some("forgejo"))],
        vec![store],
    );
    let table = Table::new(vec![(
        (
            0,
            "acme/widget",
            "pinned:0123456789abcdef0123456789abcdef01234567",
        ),
        Outcome::Hit,
    )]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::Routed);
    assert_eq!(
        response.decisions[0].via.as_deref(),
        Some("file:///srv/repos/acme/widget.git")
    );
    assert!(response.credentials.is_empty());
}

#[test]
fn invalid_requests_fail_closed() {
    let good_store = || forgejo_store("forgejo", vec!["acme"]);
    let good_repo = || repo("a", "acme/widget", pinned(), Some("forgejo"));
    // Cleartext canonical base.
    let mut req = request(vec![good_repo()], vec![good_store()]);
    req.canonical_base = "http://pointer.example".into();
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    // Uppercase / short hash.
    let mut req = request(vec![good_repo()], vec![good_store()]);
    req.repositories[0].r#ref = RefKind::Pinned("ABC".into());
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    // Moving ref without refs/ prefix.
    let mut req = request(vec![good_repo()], vec![good_store()]);
    req.repositories[0].r#ref = RefKind::Moving("main".into());
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    // Filesystem store without opt-in.
    let mut req = request(vec![good_repo()], vec![good_store()]);
    req.stores[0] = Store {
        kind: StoreKind::Filesystem,
        location: "/srv/repos".into(),
        identity: "forgejo".into(),
        scope: vec!["acme".into()],
        provider: None,
        credential_env: None,
        username: None,
        trusted_single_user: false,
    };
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    // Token value smuggled as env name.
    let mut req = request(vec![good_repo()], vec![good_store()]);
    req.stores[0].credential_env = Some("ActualTokenValue123".into());
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    // Shell-breaking username.
    let mut req = request(vec![good_repo()], vec![good_store()]);
    req.stores[0].username = Some("-evil;id".into());
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    // Filesystem root with URL-significant characters.
    let mut req = request(vec![good_repo()], vec![good_store()]);
    req.stores[0] = Store {
        kind: StoreKind::Filesystem,
        location: "/srv/repo#s".into(),
        identity: "forgejo".into(),
        scope: vec!["acme".into()],
        provider: None,
        credential_env: None,
        username: None,
        trusted_single_user: true,
    };
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    // Duplicate ids, bad timeout, empty scope, bad schema.
    let req = request(vec![good_repo(), good_repo()], vec![good_store()]);
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    let mut req = request(vec![good_repo()], vec![good_store()]);
    req.timeout_secs = 0;
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    let mut req = request(vec![good_repo()], vec![good_store()]);
    req.stores[0].scope.clear();
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    let mut req = request(vec![good_repo()], vec![good_store()]);
    req.schema = 2;
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    // Unknown fields rejected at parse time.
    let raw = serde_json::json!({
        "schema": 1,
        "canonical_base": "https://pointer.example",
        "repositories": [{"id": "a", "path": "acme/widget",
                          "ref": {"pinned": SHA}, "primary": "forgejo",
                          "source_urls": ["https://pointer.example/acme/widget"]}],
        "stores": [],
        "timeout_secs": 30,
        "operator_host": "evil.example"
    });
    assert!(serde_json::from_value::<Request>(raw).is_err());
}

#[test]
fn source_urls_must_be_declared_completely_and_consistently() {
    let good_store = || forgejo_store("forgejo", vec!["acme"]);
    // Empty inventory fails closed.
    let mut req = request(
        vec![repo("a", "acme/widget", pinned(), Some("forgejo"))],
        vec![good_store()],
    );
    req.repositories[0].source_urls.clear();
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    // Cross-repo declaration fails closed.
    let mut req = request(
        vec![repo("a", "acme/widget", pinned(), Some("forgejo"))],
        vec![good_store()],
    );
    req.repositories[0]
        .source_urls
        .push("https://pointer.example/acme/other".into());
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
    // Query strings and userinfo are not fetch identities.
    for bad in [
        "https://pointer.example/acme/widget?x=1",
        "https://user@pointer.example/acme/widget",
    ] {
        let mut req = request(
            vec![repo("a", "acme/widget", pinned(), Some("forgejo"))],
            vec![good_store()],
        );
        req.repositories[0].source_urls.push(bad.into());
        assert!(clmr::select(&req, &Table::new(vec![])).is_err(), "{bad}");
    }
    // Undeclared alias host does not normalize.
    let mut req = request(
        vec![repo("a", "acme/widget", pinned(), Some("forgejo"))],
        vec![good_store()],
    );
    req.repositories[0]
        .source_urls
        .push("https://github.com/acme/widget".into());
    assert!(clmr::select(&req, &Table::new(vec![])).is_err());
}

#[test]
fn contract_alias_example_validates_and_selects() {
    // Regression for runtime CONTRACT-FEEDBACK: the contract's own alias
    // shape (`https://github.com/acme`) must validate, and an aliased
    // request must select end-to-end (here with zero stores: pointer plus
    // alias normalization, no probes).
    let req = Request {
        schema: 1,
        canonical_base: "https://pointer.example".into(),
        aliases: aliases(),
        repositories: vec![aliased_repo("w", "acme/widget", pinned(), Some("forgejo"))],
        stores: vec![],
        timeout_secs: 30,
        primary_source: None,
    };
    let table = Table::new(vec![]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::CanonicalPointer);
    assert!(table.calls.borrow().is_empty());
    let pairs = &response.decisions[0].instead_of;
    assert!(pairs.contains(&(
        "https://github.com/acme/widget".into(),
        "https://pointer.example/acme/widget".into()
    )));
}

#[test]
fn zero_stores_with_aliases_normalizes_without_probes() {
    let mut req = request(
        vec![aliased_repo("a", "acme/widget", pinned(), Some("forgejo"))],
        vec![],
    );
    req.aliases = aliases();
    let table = Table::new(vec![]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::CanonicalPointer);
    assert!(table.calls.borrow().is_empty());
    // All four declared forms are emitted, including the bare canonical
    // identity guard; alias forms normalize to the canonical pointer.
    let pairs = &response.decisions[0].instead_of;
    assert_eq!(pairs.len(), 4);
    assert!(pairs.contains(&(
        "https://pointer.example/acme/widget".into(),
        "https://pointer.example/acme/widget".into()
    )));
    assert!(pairs.contains(&(
        "https://github.com/acme/widget".into(),
        "https://pointer.example/acme/widget".into()
    )));
    assert!(pairs.contains(&(
        "https://github.com/acme/widget.git".into(),
        "https://pointer.example/acme/widget".into()
    )));
    assert!(response
        .git_config
        .contains("insteadOf = https://github.com/acme/widget"));
    assert!(response
        .git_config
        .contains("insteadOf = https://pointer.example/acme/widget"));
    assert!(!response.git_config.contains("forge.example"));
}

#[test]
fn declared_pointer_neighbor_guards_longer_match() {
    // Routed widget plus pointer neighbor widget-evil (non-Forgejo primary).
    let mut req = request(
        vec![
            aliased_repo("w", "acme/widget", pinned(), Some("forgejo")),
            aliased_repo("e", "acme/widget-evil", branch(), Some("github")),
        ],
        vec![forgejo_store("forgejo", vec!["acme"])],
    );
    req.aliases = aliases();
    let table = Table::new(vec![(
        (
            0,
            "acme/widget",
            "pinned:0123456789abcdef0123456789abcdef01234567",
        ),
        Outcome::Hit,
    )]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::Routed);
    assert_eq!(response.decisions[1].outcome, Routing::CanonicalPointer);
    // The neighbor's declared forms stay under the canonical pointer, never
    // the store: git's longest match prefers these over the shorter routed key.
    for (from, to) in &response.decisions[1].instead_of {
        assert!(to.starts_with("https://pointer.example/"), "{from} -> {to}");
        assert!(!to.contains("forge.example"), "{from} -> {to}");
    }
    let neighbor_froms: Vec<_> = response.decisions[1]
        .instead_of
        .iter()
        .map(|(f, _)| f.as_str())
        .collect();
    assert!(neighbor_froms.contains(&"https://pointer.example/acme/widget-evil.git"));
    assert!(neighbor_froms.contains(&"https://github.com/acme/widget-evil"));
    // And the routed repo keeps all four declared forms.
    assert_eq!(response.decisions[0].instead_of.len(), 4);
}

#[test]
fn primary_url_is_metadata_only_never_a_target() {
    let mut r = repo(
        "a",
        "acme/widget",
        RefKind::Pinned(SHA2.into()),
        Some("github"),
    );
    r.primary_url = Some("https://github.com/acme/widget".into());
    let req = request(vec![r], vec![forgejo_store("forgejo", vec!["other"])]);
    let table = Table::new(vec![]);
    let response = clmr::select(&req, &table).unwrap();
    assert_eq!(response.decisions[0].outcome, Routing::CanonicalPointer);
    assert_eq!(
        response.decisions[0].primary_url.as_deref(),
        Some("https://github.com/acme/widget")
    );
    assert!(!response.git_config.contains("github.com"));
}

#[test]
fn response_round_trips_with_wire_stable_outcomes() {
    // Regression: `outcome` was `&'static str`, which cannot satisfy
    // `DeserializeOwned` for real consumers. Responses must serialize to
    // the contract strings and deserialize back.
    let req = request(
        vec![
            repo("a", "acme/widget", pinned(), Some("forgejo")),
            repo("b", "acme/gadget", branch(), None),
        ],
        vec![forgejo_store("forgejo", vec!["acme"])],
    );
    let table = Table::new(vec![(
        (
            0,
            "acme/widget",
            "pinned:0123456789abcdef0123456789abcdef01234567",
        ),
        Outcome::Hit,
    )]);
    let response = clmr::select(&req, &table).unwrap();
    let raw = serde_json::to_string(&response).unwrap();
    assert!(raw.contains("\"outcome\":\"routed\""));
    assert!(raw.contains("\"outcome\":\"canonical-pointer\""));
    let back: clmr::Response = serde_json::from_str(&raw).unwrap();
    assert_eq!(back.decisions[0].outcome, Routing::Routed);
    assert_eq!(back.decisions[1].outcome, Routing::CanonicalPointer);
    assert_eq!(
        back.decisions[0].instead_of,
        response.decisions[0].instead_of
    );
}

#[test]
fn contract_example_round_trips() {
    let raw = serde_json::json!({
        "schema": 1,
        "canonical_base": "https://pointer.example",
        "aliases": [{"url_prefix": "https://github.com/acme", "canonical_owner": "acme"}],
        "repositories": [
            {"id": "widget", "path": "acme/widget",
             "ref": {"pinned": SHA}, "primary": "forgejo",
             "primary_url": "https://github.com/acme/widget",
             "source_urls": ["https://pointer.example/acme/widget",
                             "https://pointer.example/acme/widget.git"]}
        ],
        "stores": [{"kind": "http-forge", "provider": "forgejo",
                    "location": "https://forge.example:3001",
                    "identity": "forgejo", "scope": ["acme"],
                    "credential_env": "CFRG_RESOLVER_FORGEJO_TOKEN",
                    "username": "ci"}],
        "timeout_secs": 30
    });
    let req: Request = serde_json::from_value(raw).unwrap();
    let table = Table::new(vec![(
        (
            0,
            "acme/widget",
            "pinned:0123456789abcdef0123456789abcdef01234567",
        ),
        Outcome::Hit,
    )]);
    let response = clmr::select(&req, &table).unwrap();
    let rendered = serde_json::to_string_pretty(&response).unwrap();
    assert!(rendered.contains("https://forge.example:3001/acme/widget.git"));
    assert!(response.git_config.contains("insteadOf"));
}
