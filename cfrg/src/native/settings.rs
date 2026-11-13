//! R42 of the operating model: the settings every copy carries. The decision is pure. The
//! Forgejo primary says what a copy must carry, the copy shows what it carries, and this
//! says what to change, what to hold back and what the forge cannot hold at all. Adapters
//! read and write; nothing here does I/O.
//!
//! Visibility has a direction. A copy that is more open than its primary is narrowed at
//! once (the safe direction). A copy that would have to become public is never written by
//! this pass: publishing cannot be undone (caches, indexes), so it is held and reported.
use super::{Endpoint, Provider};
use serde::Serialize;
use serde_json::{json, Value};

/// A copy to read or write: its forge, the credentialed API endpoint and its path.
#[derive(Debug, Clone)]
pub struct Target {
    pub provider: Provider,
    pub endpoint: Endpoint,
    /// `owner/repo` on that forge.
    pub path: String,
}

/// What an adapter did with one planned change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Applied {
    pub field: Field,
    /// `applied`, or `skipped` with the reason.
    pub outcome: &'static str,
    pub reason: Option<&'static str>,
}

impl Applied {
    pub fn done(field: Field) -> Self {
        Self {
            field,
            outcome: "applied",
            reason: None,
        }
    }
    pub fn skipped(field: Field, reason: &'static str) -> Self {
        Self {
            field,
            outcome: "skipped",
            reason: Some(reason),
        }
    }
}

/// What the Forgejo primary says a copy must match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Primary {
    pub private: bool,
    pub description: String,
    /// Forgejo calls it the website.
    pub homepage: String,
    pub topics: Vec<String>,
    pub default_branch: String,
    pub archived: bool,
}

/// What a copy shows, normalised by its adapter. `None`: the forge does not report it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    pub private: Option<bool>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub topics: Option<Vec<String>>,
    pub default_branch: Option<String>,
    pub archived: Option<bool>,
    /// The forge's issue tracker is on (R38).
    pub issues: Option<bool>,
    /// The forge's wiki is on (R40).
    pub wiki: Option<bool>,
    /// The forge's own CI runs on the copy (R6).
    pub ci: Option<bool>,
    /// Automatic dependency or security update tooling is on (R35).
    pub updates: Option<bool>,
    /// Private vulnerability reporting is on (GitHub).
    pub vulnerability_reporting: Option<bool>,
}

/// One setting. The declaration order is the order of application: a copy is unarchived
/// before anything else is written, and archived only after everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Field {
    Unarchive,
    Visibility,
    Description,
    Homepage,
    Topics,
    DefaultBranch,
    Issues,
    Wiki,
    Ci,
    Updates,
    VulnerabilityReporting,
    Archive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub field: Field,
    pub from: Value,
    pub to: Value,
}

/// A difference this pass does not write, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Held {
    pub field: Field,
    pub reason: &'static str,
    pub wanted: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// In application order.
    pub changes: Vec<Change>,
    pub held: Vec<Held>,
    /// What the forge cannot hold at all (R3 "as closely as the forge allows").
    pub unsupported: Vec<Field>,
}

impl Plan {
    pub fn converged(&self) -> bool {
        self.changes.is_empty() && self.held.is_empty()
    }
    pub fn to_json(&self) -> Value {
        json!({"changes":self.changes, "held":self.held, "unsupported":self.unsupported})
    }
}

/// How well the default branch of a copy is held for the mirroring alone (R43). Read-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Protection {
    /// The receiver lock is in place exactly as cfrg writes it.
    Locked,
    /// Some protection exists on the default branch, but it is not the receiver lock.
    Restricted { detail: String },
    /// The default branch is open to anyone with write access.
    Missing,
    /// The forge's tier cannot lock this copy, or the question was not asked.
    Unavailable { reason: String },
}

/// What a forge can store.
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub homepage: bool,
    pub topics: bool,
    pub archive: bool,
    /// The forge has an issue tracker and a wiki at all. Bitbucket Cloud removed both on
    /// 2026-08-20; a copy there carries links in its description instead.
    pub issues: bool,
    pub wiki: bool,
    /// Automatic update tooling can be switched off through the API.
    pub updates: bool,
    /// Private vulnerability reporting can be switched on through the API.
    pub vulnerability_reporting: bool,
    pub description_limit: usize,
    pub topic_limit: usize,
}

pub fn capabilities(provider: Provider) -> Capabilities {
    match provider {
        Provider::Github => Capabilities {
            homepage: true,
            topics: true,
            archive: true,
            issues: true,
            wiki: true,
            updates: true,
            vulnerability_reporting: true,
            description_limit: 350,
            topic_limit: 20,
        },
        Provider::Gitlab => Capabilities {
            homepage: false,
            topics: true,
            archive: true,
            issues: true,
            wiki: true,
            updates: false,
            vulnerability_reporting: false,
            description_limit: 2000,
            topic_limit: usize::MAX,
        },
        Provider::Bitbucket => Capabilities {
            homepage: true,
            topics: false,
            archive: false,
            issues: false,
            wiki: false,
            updates: false,
            vulnerability_reporting: false,
            description_limit: usize::MAX,
            topic_limit: 0,
        },
    }
}

/// Whether a copy of this forge carries a wiki (owner rule R40: the wiki is copied wherever
/// that is free). GitLab does. A PUBLIC GitHub copy does (the Forgejo wiki is pushed to its
/// `<repo>.wiki.git` through the App installation token, fast-forward only); a PRIVATE GitHub
/// copy does not, and its description links to the Forgejo wiki. Bitbucket has no wiki since
/// 2026-08-20.
pub fn wiki_wanted(provider: Provider, private: bool) -> Option<bool> {
    match provider {
        Provider::Github => Some(!private),
        Provider::Gitlab => Some(true),
        Provider::Bitbucket => None,
    }
}

/// A topic as GitHub accepts it: lowercase letters, digits and hyphens, at most 50.
fn github_topic(topic: &str) -> bool {
    !topic.is_empty()
        && topic.len() <= 50
        && !topic.starts_with('-')
        && topic
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn normalized(topics: &[String]) -> Vec<String> {
    let mut all: Vec<String> = topics
        .iter()
        .map(|t| t.trim().to_ascii_lowercase())
        .collect();
    all.sort_unstable();
    all.dedup();
    all
}

/// Decide what a copy needs.
pub fn plan(provider: Provider, primary: &Primary, seen: &Observed) -> Plan {
    plan_with_wiki_repo(provider, primary, seen, true)
}

/// [`plan`] with GitHub's `<repo>.wiki.git` state: GitHub creates it with the first page, a
/// web-UI act nobody scripts (R9), so an uninitialised one counts as no wiki (like a private copy).
pub fn plan_with_wiki_repo(
    provider: Provider,
    primary: &Primary,
    seen: &Observed,
    wiki_repo: bool,
) -> Plan {
    let caps = capabilities(provider);
    let mut plan = Plan::default();
    let change = |plan: &mut Plan, field, from: Value, to: Value| {
        plan.changes.push(Change { field, from, to });
    };

    // Visibility first: it decides what the forge lets the copy have (a GitHub wiki).
    let mut effective_private = seen.private.unwrap_or(primary.private);
    if let Some(live) = seen.private {
        if live != primary.private {
            if primary.private {
                change(
                    &mut plan,
                    Field::Visibility,
                    json!("public"),
                    json!("private"),
                );
                effective_private = true;
            } else {
                plan.held.push(Held {
                    field: Field::Visibility,
                    reason: "publishing-needs-approval",
                    wanted: json!("public"),
                });
            }
        }
    }

    if let Some(live) = &seen.description {
        let wanted = primary.description.trim();
        if live.trim() != wanted {
            if wanted.chars().count() > caps.description_limit {
                plan.held.push(Held {
                    field: Field::Description,
                    reason: "longer-than-the-forge-allows",
                    wanted: json!(wanted),
                });
            } else {
                change(&mut plan, Field::Description, json!(live), json!(wanted));
            }
        }
    }

    if !caps.homepage {
        if !primary.homepage.trim().is_empty() {
            plan.unsupported.push(Field::Homepage);
        }
    } else if let Some(live) = &seen.homepage {
        if live.trim() != primary.homepage.trim() {
            change(
                &mut plan,
                Field::Homepage,
                json!(live),
                json!(primary.homepage.trim()),
            );
        }
    }

    if !caps.topics {
        if !primary.topics.is_empty() {
            plan.unsupported.push(Field::Topics);
        }
    } else if let Some(live) = &seen.topics {
        let wanted = normalized(&primary.topics);
        if normalized(live) != wanted {
            let bad = provider == Provider::Github
                && (wanted.len() > caps.topic_limit || wanted.iter().any(|t| !github_topic(t)));
            if bad {
                plan.held.push(Held {
                    field: Field::Topics,
                    reason: "topic-the-forge-does-not-accept",
                    wanted: json!(wanted),
                });
            } else {
                change(
                    &mut plan,
                    Field::Topics,
                    json!(normalized(live)),
                    json!(wanted),
                );
            }
        }
    }

    if let Some(live) = &seen.default_branch {
        if *live != primary.default_branch {
            change(
                &mut plan,
                Field::DefaultBranch,
                json!(live),
                json!(primary.default_branch),
            );
        }
    }

    // Issues are on everywhere (R38), except on GitHub: those switch on in one pass once the
    // relay's contribution and issue flows are live (owner decision 2026-11-13).
    if caps.issues && provider != Provider::Github && seen.issues == Some(false) {
        change(&mut plan, Field::Issues, json!(false), json!(true));
    }
    // The wiki is on wherever it is copied (R40); a private GitHub copy links to it instead.
    let wiki_wanted_here = wiki_wanted(provider, effective_private).map(|w| {
        // A GitHub copy holds the wiki only while its wiki repository exists; otherwise it is
        // treated like a private copy (wiki off, the description links to Forgejo).
        w && (provider != Provider::Github || wiki_repo)
    });
    if let (Some(wanted), Some(live)) = (wiki_wanted_here, seen.wiki) {
        if caps.wiki && live != wanted {
            change(&mut plan, Field::Wiki, json!(live), json!(wanted));
        }
    }
    // Private vulnerability reporting is on for a public GitHub copy.
    if caps.vulnerability_reporting
        && !effective_private
        && seen.vulnerability_reporting == Some(false)
    {
        change(
            &mut plan,
            Field::VulnerabilityReporting,
            json!(false),
            json!(true),
        );
    }
    // No CI on any copy (R6, R7): there is no exception to declare.
    if seen.ci == Some(true) {
        change(&mut plan, Field::Ci, json!("on"), json!("off"));
    }
    // No automatic update tooling on a copy (R35).
    if seen.updates == Some(true) {
        if caps.updates {
            change(&mut plan, Field::Updates, json!("on"), json!("off"));
        } else {
            plan.unsupported.push(Field::Updates);
        }
    }

    match (seen.archived, primary.archived) {
        (Some(true), false) => change(&mut plan, Field::Unarchive, json!(true), json!(false)),
        (Some(false), true) if caps.archive => {
            change(&mut plan, Field::Archive, json!(false), json!(true))
        }
        (Some(false), true) => plan.unsupported.push(Field::Archive),
        _ => {}
    }
    // A copy that stays archived is read-only: any other difference waits for the owner.
    if seen.archived == Some(true) && primary.archived && !plan.changes.is_empty() {
        for c in plan.changes.drain(..) {
            plan.held.push(Held {
                field: c.field,
                reason: "archived-copy-is-read-only",
                wanted: c.to,
            });
        }
    }
    plan.changes.sort_by_key(|c| c.field);
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primary() -> Primary {
        Primary {
            private: false,
            description: "A tool".into(),
            homepage: "https://example.org".into(),
            topics: vec!["rust".into(), "cli".into()],
            default_branch: "main".into(),
            archived: false,
        }
    }

    fn matching() -> Observed {
        Observed {
            private: Some(false),
            description: Some("A tool".into()),
            homepage: Some("https://example.org".into()),
            topics: Some(vec!["cli".into(), "rust".into()]),
            default_branch: Some("main".into()),
            archived: Some(false),
            issues: Some(true),
            // The wiki differs by forge: tests set it where it matters.
            wiki: None,
            ci: Some(false),
            updates: Some(false),
            vulnerability_reporting: Some(true),
        }
    }

    #[test]
    fn an_uninitialised_github_wiki_is_treated_like_a_private_copy() {
        let mut seen = matching();
        seen.wiki = Some(true);
        let plan = plan_with_wiki_repo(Provider::Github, &primary(), &seen, false);
        let wiki = plan
            .changes
            .iter()
            .find(|c| c.field == Field::Wiki)
            .unwrap();
        assert_eq!(wiki.to, json!(false));
        // Once a human made the first page the next run keeps the wiki on, no hold state.
        assert!(plan_for(Provider::Github, &primary(), &seen)
            .changes
            .is_empty());
    }

    #[test]
    fn a_matching_copy_is_converged() {
        for provider in [Provider::Github, Provider::Gitlab, Provider::Bitbucket] {
            let mut seen = matching();
            // The wiki is off on GitHub, on on GitLab, absent on Bitbucket.
            seen.wiki = wiki_wanted(provider, primary().private);
            let plan = plan(provider, &primary(), &seen);
            assert!(plan.changes.is_empty(), "{provider:?}: {plan:?}");
            assert!(plan.held.is_empty());
        }
    }

    #[test]
    fn a_copy_more_open_than_its_primary_is_narrowed_at_once() {
        let mut p = primary();
        p.private = true;
        let mut seen = matching();
        seen.private = Some(false);
        let plan = plan(Provider::Gitlab, &p, &seen);
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].field, Field::Visibility);
        assert_eq!(plan.changes[0].to, json!("private"));
        assert!(plan.held.is_empty());
    }

    #[test]
    fn a_copy_that_would_have_to_become_public_is_held_never_changed() {
        let mut seen = matching();
        seen.private = Some(true);
        let plan = plan(Provider::Github, &primary(), &seen);
        assert!(plan.changes.iter().all(|c| c.field != Field::Visibility));
        assert_eq!(plan.held.len(), 1);
        assert_eq!(plan.held[0].field, Field::Visibility);
        assert_eq!(plan.held[0].reason, "publishing-needs-approval");
    }

    #[test]
    fn unarchive_comes_first_and_archive_comes_last() {
        let mut seen = matching();
        seen.archived = Some(true);
        seen.description = Some("old".into());
        let plan = plan(Provider::Github, &primary(), &seen);
        // Archived and primary live: unarchive, then everything else.
        let fields: Vec<_> = plan.changes.iter().map(|c| c.field).collect();
        assert_eq!(fields, [Field::Unarchive, Field::Description]);

        let mut p = primary();
        p.archived = true;
        let mut seen = matching();
        seen.description = Some("old".into());
        let plan = plan_for(Provider::Gitlab, &p, &seen);
        let fields: Vec<_> = plan.changes.iter().map(|c| c.field).collect();
        assert_eq!(fields, [Field::Description, Field::Archive]);
    }

    fn plan_for(provider: Provider, p: &Primary, seen: &Observed) -> Plan {
        plan(provider, p, seen)
    }

    #[test]
    fn an_archived_copy_of_an_archived_primary_is_read_only() {
        let mut p = primary();
        p.archived = true;
        let mut seen = matching();
        seen.archived = Some(true);
        seen.description = Some("old".into());
        let plan = plan_for(Provider::Github, &p, &seen);
        assert!(plan.changes.is_empty());
        assert_eq!(plan.held[0].reason, "archived-copy-is-read-only");
    }

    #[test]
    fn what_a_forge_cannot_hold_is_reported_not_forced() {
        // GitLab has no homepage; Bitbucket has no topics and no archive.
        let plan = plan_for(Provider::Gitlab, &primary(), &matching());
        assert_eq!(plan.unsupported, [Field::Homepage]);
        let mut p = primary();
        p.archived = true;
        let plan = plan_for(Provider::Bitbucket, &p, &matching());
        assert!(plan.unsupported.contains(&Field::Topics));
        assert!(plan.unsupported.contains(&Field::Archive));
        assert!(plan.changes.iter().all(|c| c.field != Field::Archive));
    }

    #[test]
    fn topics_compare_as_sets_and_github_refuses_what_it_cannot_store() {
        let mut p = primary();
        p.topics = vec!["Rust".into(), "cli".into(), "rust".into()];
        let mut seen = matching();
        seen.topics = Some(vec!["cli".into(), "rust".into()]);
        assert!(plan_for(Provider::Github, &p, &seen).changes.is_empty());

        p.topics = vec!["a.b".into()];
        let plan = plan_for(Provider::Github, &p, &seen);
        assert_eq!(plan.held[0].reason, "topic-the-forge-does-not-accept");
        // GitLab takes any topic.
        assert_eq!(plan_for(Provider::Gitlab, &p, &seen).changes.len(), 1);
    }

    #[test]
    fn a_description_over_the_forge_limit_is_held() {
        let mut p = primary();
        p.description = "x".repeat(351);
        let plan = plan_for(Provider::Github, &p, &matching());
        assert_eq!(plan.held[0].reason, "longer-than-the-forge-allows");
        assert!(plan_for(Provider::Gitlab, &p, &matching())
            .changes
            .iter()
            .any(|c| c.field == Field::Description));
    }

    #[test]
    fn issues_wiki_ci_and_updates_follow_the_model() {
        let mut seen = matching();
        seen.issues = Some(false);
        seen.wiki = Some(false);
        seen.ci = Some(true);
        seen.updates = Some(true);
        let fields = |plan: Plan| plan.changes.iter().map(|c| c.field).collect::<Vec<_>>();
        // GitHub: issues untouched for now, a public copy's wiki ON, CI off, update tooling off.
        let github = plan_for(Provider::Github, &primary(), &seen);
        assert_eq!(
            fields(github.clone()),
            [Field::Wiki, Field::Ci, Field::Updates]
        );
        let wiki = github
            .changes
            .iter()
            .find(|c| c.field == Field::Wiki)
            .unwrap();
        assert_eq!(wiki.to, json!(true));
        // CI goes off on every forge that reports it; nothing can keep it on.
        let gitlab = plan_for(Provider::Gitlab, &primary(), &seen);
        assert!(gitlab
            .changes
            .iter()
            .any(|c| c.field == Field::Ci && c.to == json!("off")));
        // GitLab cannot switch update tooling off through the API.
        assert!(gitlab.unsupported.contains(&Field::Updates));
    }

    #[test]
    fn gitlab_keeps_its_wiki_on_and_turns_a_missing_one_on() {
        let mut seen = matching();
        seen.wiki = Some(false);
        let plan = plan_for(Provider::Gitlab, &primary(), &seen);
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].field, Field::Wiki);
        assert_eq!(plan.changes[0].to, json!(true));
    }

    #[test]
    fn bitbucket_has_neither_issues_nor_wiki_so_none_is_planned() {
        let mut seen = matching();
        seen.issues = Some(false);
        seen.wiki = Some(false);
        let plan = plan_for(Provider::Bitbucket, &primary(), &seen);
        assert!(plan
            .changes
            .iter()
            .all(|c| c.field != Field::Issues && c.field != Field::Wiki));
    }

    #[test]
    fn private_vulnerability_reporting_is_switched_on_for_a_public_github_copy_only() {
        let mut seen = matching();
        seen.wiki = Some(true);
        seen.vulnerability_reporting = Some(false);
        let plan = plan_for(Provider::Github, &primary(), &seen);
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].field, Field::VulnerabilityReporting);
        // A private copy has no use for it, and the other forges have no such switch.
        let mut p = primary();
        p.private = true;
        let mut seen_private = seen.clone();
        seen_private.private = Some(true);
        seen_private.wiki = Some(false);
        assert!(plan_for(Provider::Github, &p, &seen_private)
            .changes
            .is_empty());
        assert!(plan_for(Provider::Gitlab, &primary(), &seen)
            .changes
            .iter()
            .all(|c| c.field != Field::VulnerabilityReporting));
    }

    #[test]
    fn the_default_branch_follows_the_primary() {
        let mut seen = matching();
        seen.default_branch = Some("master".into());
        let plan = plan_for(Provider::Bitbucket, &primary(), &seen);
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].field, Field::DefaultBranch);
        assert_eq!(plan.changes[0].to, json!("main"));
    }
}
