//! The same fixture and spelling table as ccid's `src/contributed/tests.rs`: the two read one declaration, and a
//! change to the rules of one must be made to the other.
use super::*;

const PLACEMENT: &str = r#"{
  "version": 1,
  "default": "https://forge.example",
  "primaries": {},
  "native": {
    "schema": 1,
    "source": {"origin": "https://forge.example", "token_env": "T"},
    "repositories": [],
    "contributed": [
      {"name": "alpha", "upstream": "https://github.com/someone/alpha",
       "forks": ["https://github.com/me/alpha", "https://forge.example/me/alpha"]},
      {"name": "beta", "upstream": "https://gitlab.example/group/sub/beta"}
    ]
  }
}"#;

fn placement(text: &str) -> Placement {
    Placement::from_document(text.as_bytes()).unwrap()
}

fn index() -> Contributed {
    Contributed::from_placement(&placement(PLACEMENT), &[]).unwrap()
}

fn error_of(list: &str) -> String {
    let text = format!(
        r#"{{"native":{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"repositories":[],"contributed":{list}}}}}"#
    );
    let placement = Placement::from_document(text.as_bytes());
    match placement {
        Err(error) => error.to_string(),
        Ok(placement) => placement.validate().unwrap_err().to_string(),
    }
}

#[test]
fn every_spelling_of_a_location_has_one_key() {
    let expected = Some("github.com/someone/alpha".to_owned());
    for url in [
        "https://github.com/someone/alpha",
        "https://github.com/someone/alpha.git",
        "https://github.com/someone/alpha/",
        "HTTPS://GitHub.com/Someone/Alpha.git",
        "http://github.com/someone/alpha",
        "https://token@github.com/someone/alpha.git",
        "https://user:secret@github.com:8443/someone/alpha.git",
        "ssh://git@github.com/someone/alpha.git",
        "ssh://git@github.com:2222/someone/alpha",
        "git@github.com:someone/alpha.git",
        "github.com:someone/alpha",
        "git+https://github.com/someone/alpha",
        "  https://github.com/someone/alpha  ",
    ] {
        assert_eq!(key_of(url), expected, "{url}");
    }
    assert_eq!(
        key_of("https://gitlab.example/group/sub/beta.git").as_deref(),
        Some("gitlab.example/group/sub/beta")
    );
}

#[test]
fn what_names_no_repository_has_no_key() {
    for url in [
        "",
        "/home/me/alpha",
        "../alpha",
        "file:///srv/git/owner/alpha",
        "https://github.com/someone",
        "https://github.com/",
        "https://github.com/someone/../alpha",
        "https://github.com/some one/alpha",
        "ftp://github.com/someone/alpha",
        "https://[::1]/owner/alpha",
        "https:///owner/alpha",
    ] {
        assert_eq!(key_of(url), None, "{url:?}");
    }
}

#[test]
fn a_location_is_recognised_in_any_spelling_of_the_project() {
    let index = index();
    for url in [
        "https://github.com/someone/alpha.git",
        "git@github.com:me/alpha.git",
        "https://forge.example/me/alpha",
        "https://FORGE.example/Me/Alpha.git",
    ] {
        assert_eq!(
            index.classify_url(url).map(|p| p.name.as_str()),
            Some("alpha"),
            "{url}"
        );
    }
    assert!(index
        .classify_url("https://gitlab.example/group/sub/beta")
        .is_some());
    for url in [
        "https://github.com/someone/alpha-other",
        "https://github.com/someone/alph",
        "https://github.com/other/alpha",
        "https://gitlab.example/group/beta",
        "https://forge.example/me/other",
        "https://forge.example/someone/alpha",
    ] {
        assert!(index.classify_url(url).is_none(), "{url}");
    }
}

#[test]
fn the_router_asks_by_forge_and_path() {
    let index = index();
    // Our forge, as a host or as an origin; case and a slash around the path do not matter.
    for host in [
        "forge.example",
        "https://forge.example",
        "https://forge.example/",
        "FORGE.example",
    ] {
        assert!(index.classify(host, "me/alpha").is_some(), "{host}");
        assert!(index.classify(host, "/Me/Alpha/").is_some(), "{host}");
        assert!(index.classify(host, "me/other").is_none(), "{host}");
    }
    assert!(index.classify("github.com", "someone/alpha").is_some());
    assert!(index.classify("github.com", "me/alpha").is_some());
    // The same path on another forge is another repository.
    assert!(index.classify("elsewhere.example", "me/alpha").is_none());
    let error = index
        .refuse("relay", "forge.example", "me/alpha")
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("relay refused: alpha is a contributed project"),
        "{error}"
    );
    assert!(index.refuse("relay", "forge.example", "own/thing").is_ok());
    assert!(index
        .refuse_url("mirror", "git@github.com:me/alpha.git")
        .is_err());
    assert!(index
        .refuse_url("mirror", "git@github.com:me/own.git")
        .is_ok());
}

#[test]
fn our_forge_is_known_under_the_spellings_the_caller_names() {
    let aliased = Contributed::from_placement(&placement(PLACEMENT), &["git.example"]).unwrap();
    assert!(aliased.classify("git.example", "me/alpha").is_some());
    assert!(aliased
        .classify_url("https://git.example/me/alpha.git")
        .is_some());
    assert!(aliased.classify("other.example", "me/alpha").is_none());
    assert!(index().classify("git.example", "me/alpha").is_none());
}

#[test]
fn the_source_forge_repositories_are_the_ones_whose_actions_are_kept_off() {
    assert_eq!(index().source_paths(), ["me/alpha"]);
    // Only locations on the source forge; a nested path is not a Forgejo repository.
    let text = PLACEMENT.replace(
        r#"{"name": "beta", "upstream": "https://gitlab.example/group/sub/beta"}"#,
        r#"{"name": "beta", "upstream": "https://gitlab.example/group/sub/beta", "forks": ["https://forge.example/me/beta", "https://forge.example/me/beta-two"]}"#,
    );
    let index = Contributed::from_placement(&placement(&text), &[]).unwrap();
    assert_eq!(index.source_paths(), ["me/alpha", "me/beta", "me/beta-two"]);
    assert!(Contributed::from_placement(&placement(r#"{"native":{"schema":1,"source":{"origin":"https://forge.example","token_env":"T"},"repositories":[]}}"#), &[]).unwrap().source_paths().is_empty());
}

#[test]
fn a_malformed_declaration_is_refused_with_the_entry_named() {
    let ok = r#"{"name":"alpha","upstream":"https://github.com/o/alpha"}"#;
    let text = format!(
        r#"{{"native":{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},"repositories":[],"contributed":[{ok}]}}}}"#
    );
    placement(&text).validate().unwrap();
    for (list, wanted) in [
        (
            r#"[{"upstream":"https://github.com/o/a"}]"#,
            "missing field",
        ),
        (
            r#"[{"name":"Alpha","upstream":"https://github.com/o/a"}]"#,
            "plain lowercase name",
        ),
        (
            r#"[{"name":"-a","upstream":"https://github.com/o/a"}]"#,
            "plain lowercase name",
        ),
        (r#"[{"name":"a"}]"#, "missing field"),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a","extra":1}]"#,
            "unknown field",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a","forks":["https://github.com/o/a"]}]"#,
            "listed twice",
        ),
        (
            r#"[{"name":"a","upstream":"http://github.com/o/a"}]"#,
            "an https URL",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a.git"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a/"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://GitHub.com/o/a"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://git@github.com/o/a"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com:443/o/a"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/../a"}]"#,
            "not a canonical location",
        ),
        (
            r#"[{"name":"b","upstream":"https://github.com/o/b"},{"name":"a","upstream":"https://github.com/o/a"}]"#,
            "ascending",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a"},{"name":"a","upstream":"https://github.com/o/b"}]"#,
            "ascending",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a"},{"name":"b","upstream":"https://github.com/O/A"}]"#,
            "share github.com/o/a",
        ),
        (
            r#"[{"name":"a","upstream":"https://github.com/o/a"},{"name":"b","upstream":"https://x.example/p/b","forks":["https://github.com/o/a"]}]"#,
            "share github.com/o/a",
        ),
    ] {
        let message = error_of(list);
        assert!(
            message.contains(wanted),
            "{list}\n  -> {message}\n  wanted {wanted:?}"
        );
    }
}

#[test]
fn a_contributed_project_has_no_mirror_destination() {
    // R2: only an explicit exclusion leaves a repository out of the mirrors, and R11 makes a contributed project
    // one: a declared mirror of it contradicts the declaration.
    let repository = |destinations: &str| {
        format!(
            r#"{{"native":{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},
            "repositories":[{{"path":"me/alpha","source_id":7,"private":false,"default_branch":"main","content":"native-git","hold":null,"destinations":[{destinations}]}}],
            "contributed":[{{"name":"alpha","upstream":"https://github.com/someone/alpha","forks":["https://forge.example/me/alpha"]}}]}}}}"#
        )
    };
    let destination = r#"{"provider":"github","endpoint":{"origin":"https://api.github.com","token_env":"T"},
        "path":"me/alpha","namespace":"me","mirror_user":"m","password_env":"P","interval_seconds":600}"#;
    // On our forge, a copy of the project that is declared with no destination is fine (it is what Actions-off acts on).
    placement(&repository("")).validate().unwrap();
    let message = placement(&repository(destination))
        .validate()
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("me/alpha is the contributed project alpha")
            && message.contains("no mirror destination"),
        "{message}"
    );
}

#[test]
fn a_placement_without_the_list_declares_nothing_and_reads_as_before() {
    let text = r#"{"native":{"schema":1,"source":{"origin":"https://forge.example","token_env":"T"},"repositories":[]}}"#;
    let placement = placement(text);
    placement.validate().unwrap();
    assert!(placement.contributed.is_empty());
    let index = Contributed::from_placement(&placement, &[]).unwrap();
    assert!(index.is_empty());
    assert!(index.classify("forge.example", "me/alpha").is_none());
    // Written back, the field does not appear.
    assert!(!serde_json::to_string(&placement)
        .unwrap()
        .contains("contributed"));
}

#[test]
fn the_whole_document_and_its_native_view_declare_the_same_projects() {
    let whole = placement(PLACEMENT);
    let native =
        serde_json::from_str::<serde_json::Value>(PLACEMENT).unwrap()["native"].to_string();
    let view = placement(&native);
    assert_eq!(whole.contributed, view.contributed);
    whole.validate().unwrap();
    view.validate().unwrap();
}

#[test]
fn a_placement_file_is_loaded_and_validated() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("placement.json");
    fs::write(&file, PLACEMENT).unwrap();
    let index = Contributed::load(&file, &[]).unwrap();
    assert_eq!(index.projects().len(), 2);
    fs::write(
        &file,
        PLACEMENT.replace("someone/alpha", "someone/alpha.git"),
    )
    .unwrap();
    assert!(Contributed::load(&file, &[]).is_err());
    assert!(Contributed::load(&temp.path().join("missing.json"), &[]).is_err());
}

#[test]
fn the_default_primary_is_not_a_spelling_of_our_forge() {
    // `default` names the default PRIMARY; after a primary switch it can be any forge.
    let text = PLACEMENT.replace(
        r#""default": "https://forge.example""#,
        r#""default": "https://github.com""#,
    );
    let index = Contributed::from_placement(&placement(&text), &[]).unwrap();
    assert_eq!(index.forge_hosts(), ["forge.example"]);
    assert!(
        index.classify("github.com", "me/alpha").is_some(),
        "a listed fork is still a fork"
    );
    assert!(index.classify("github.com", "own/thing").is_none());
}

#[test]
fn a_providers_api_origin_names_its_web_host() {
    let index = index();
    assert!(index.classify("api.github.com", "me/alpha").is_some());
    assert!(index
        .classify("https://api.github.com", "someone/alpha")
        .is_some());
    assert!(index.classify("api.bitbucket.org", "me/alpha").is_none());
}

fn with_repositories(repositories: &str) -> String {
    format!(
        r#"{{"native":{{"schema":1,"source":{{"origin":"https://forge.example","token_env":"T"}},
        "repositories":[{repositories}],
        "contributed":[{{"name":"alpha","upstream":"https://github.com/someone/alpha","forks":["https://github.com/me/alpha","https://forge.example/me/alpha"]}}]}}}}"#
    )
}

fn repository(path: &str, destinations: &str) -> String {
    format!(
        r#"{{"path":"{path}","source_id":7,"private":false,"default_branch":"main","content":"native-git","hold":null,"destinations":[{destinations}]}}"#
    )
}

fn destination(provider: &str, origin: &str, path: &str, extra: &str) -> String {
    // A GitHub destination names its owner as its namespace.
    let owner = path.split('/').next().unwrap_or_default();
    format!(
        r#"{{"provider":"{provider}","endpoint":{{"origin":"{origin}","token_env":"T"}},"path":"{path}","namespace":"{owner}","mirror_user":"m","password_env":"P","interval_seconds":600{extra}}}"#
    )
}

#[test]
fn nothing_is_mirrored_into_a_contributed_project() {
    // An owned repository whose pinned destination IS the contributed fork on GitHub.
    let pinned = destination(
        "github",
        "https://api.github.com",
        "me/alpha",
        r#","path_reason":"patches live in the fork""#,
    );
    let text = with_repositories(&repository("me/patches", &pinned));
    let message = placement(&text).validate().unwrap_err().to_string();
    assert!(
        message.contains("me/patches would be mirrored into github.com/me/alpha")
            && message.contains("contributed project alpha"),
        "{message}"
    );
    // A GitLab destination is matched by its own host.
    let gitlab = destination(
        "gitlab",
        "https://gitlab.example",
        "group/sub/beta",
        r#","path_reason":"a pinned name""#,
    );
    let text = with_repositories(&repository("own/thing", &gitlab)).replace(
        r#""contributed":["#,
        r#""contributed":[{"name":"aaa","upstream":"https://gitlab.example/group/sub/beta"},"#,
    );
    let message = placement(&text).validate().unwrap_err().to_string();
    assert!(
        message.contains("gitlab.example/group/sub/beta"),
        "{message}"
    );
    // An ordinary repository with an ordinary destination is untouched.
    let fine = destination("github", "https://api.github.com", "own/thing", "");
    placement(&with_repositories(&repository("own/thing", &fine)))
        .validate()
        .unwrap();
}

#[test]
fn a_mirror_that_is_being_retired_may_stay_declared() {
    // How an existing mirror of a project that is now contributed goes: the destination is marked absent, the
    // reconciler removes the Forgejo push mirror, and a later edit drops the entry.
    let retiring = destination(
        "gitlab",
        "https://gitlab.example",
        "me/alpha",
        r#","absent":true,"remote_name":"remote_mirror_x""#,
    );
    placement(&with_repositories(&repository("me/alpha", &retiring)))
        .validate()
        .unwrap();
    // A live destination beside it is still refused.
    let live = destination("gitlab", "https://gitlab.example", "me/other", "");
    let both = format!("{retiring},{live}");
    assert!(
        placement(&with_repositories(&repository("me/alpha", &both)))
            .validate()
            .is_err()
    );
}
