//! What a copy's description must link to (operating model R40, R42; Forge-router §5.9).
//!
//! A copy carries the primary's description. Where the copy's forge cannot show the issues
//! or the wiki itself, the description also links to a forge where the same readers can see
//! them. The decision is pure: the settings sync of the copies (R42) is the ONE writer of
//! descriptions and takes the wanted description from [`copy_description`].
//!
//! | Copy | Issues | Wiki (only when the primary has wiki pages) |
//! |---|---|---|
//! | GitLab | on the copy (R38): no link | copied by `cfrg native --operation wiki`: no link |
//! | GitHub | issues left as they are for now | public: pushed to `<repo>.wiki.git` by the wiki sync, no link; private: no wiki, link to Forgejo |
//! | Bitbucket | none (Bitbucket Cloud removed Issues on 2026-08-20): link | none (Wikis removed the same day): link |
//!
//! A link from a public Bitbucket copy goes to the GitHub copy where the public can see that
//! copy and it holds the object, otherwise to Forgejo. A private copy always links to Forgejo,
//! where the members of the private copy have access (R43).
use super::Provider;
use serde::Serialize;

/// The separator between the primary's description and each link.
pub const SEPARATOR: &str = " · ";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Issues,
    Wiki,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Issues => "Issues",
            Kind::Wiki => "Wiki",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Link {
    pub kind: Kind,
    pub url: String,
}

/// The GitHub copy of the same primary, when anyone can see it: it exists, it is public and
/// its organisation is neither flagged nor paused by the write gate. Pass `None` otherwise;
/// a link then goes to Forgejo, which is always right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicGithub<'a> {
    /// `owner/repo` on GitHub.
    pub path: &'a str,
    /// Its issue tracker is on.
    pub issues: bool,
    /// It holds the wiki (has wiki pages). Today never true, see the module table.
    pub wiki: bool,
}

/// What decides the links of one copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts<'a> {
    /// The copy's forge.
    pub provider: Provider,
    /// The copy is private (it has the primary's visibility, R42).
    pub private: bool,
    /// The Forgejo origin, e.g. `https://forge.corbet.ch` (the placement's source origin).
    pub forge_origin: &'a str,
    /// The primary's `owner/repo` on Forgejo.
    pub source_path: &'a str,
    /// The primary has its issue tracker on.
    pub issues: bool,
    /// The primary has wiki pages (`has_wiki` and the wiki git repository exists).
    pub wiki: bool,
    /// See [`PublicGithub`]. Only read for a public Bitbucket copy.
    pub github: Option<PublicGithub<'a>>,
}

fn forgejo(facts: &Facts, kind: Kind) -> String {
    let base = format!(
        "{}/{}",
        facts.forge_origin.trim_end_matches('/'),
        facts.source_path
    );
    match kind {
        Kind::Issues => format!("{base}/issues"),
        Kind::Wiki => format!("{base}/wiki"),
    }
}

/// Where a link of this kind points from a copy that cannot show the object itself.
fn target(facts: &Facts, kind: Kind) -> String {
    if !facts.private {
        if let Some(gh) = facts.github {
            let holds = match kind {
                Kind::Issues => gh.issues,
                Kind::Wiki => gh.wiki,
            };
            if holds {
                let base = format!("https://github.com/{}", gh.path);
                return match kind {
                    Kind::Issues => format!("{base}/issues"),
                    Kind::Wiki => format!("{base}/wiki"),
                };
            }
        }
    }
    forgejo(facts, kind)
}

/// The links a copy must carry, issues first.
pub fn links(facts: &Facts) -> Vec<Link> {
    let (needs_issues, needs_wiki) = match facts.provider {
        Provider::Gitlab => (false, false),
        // A public GitHub copy holds the wiki itself (R40); a private one links to Forgejo.
        Provider::Github => (false, facts.private),
        Provider::Bitbucket => (true, true),
    };
    let mut out = Vec::new();
    if needs_issues && facts.issues {
        out.push(Link {
            kind: Kind::Issues,
            url: target(facts, Kind::Issues),
        });
    }
    if needs_wiki && facts.wiki {
        // Only a private GitHub copy gets here, and it never holds the wiki: the link goes to Forgejo.
        let url = if facts.provider == Provider::Github {
            forgejo(facts, Kind::Wiki)
        } else {
            target(facts, Kind::Wiki)
        };
        out.push(Link {
            kind: Kind::Wiki,
            url,
        });
    }
    out
}

/// The primary's description followed by each link as `Label: url`, joined by
/// [`SEPARATOR`]. Never shortened: a result longer than the forge allows is the settings
/// planner's to hold (Q6, 2026-11-13).
pub fn with_links(primary_description: &str, links: &[Link]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let description = primary_description.trim();
    if !description.is_empty() {
        parts.push(description.to_string());
    }
    for link in links {
        parts.push(format!("{}: {}", link.kind.label(), link.url));
    }
    parts.join(SEPARATOR)
}

/// The description a copy must carry: [`with_links`] over [`links`].
pub fn copy_description(primary_description: &str, facts: &Facts) -> String {
    with_links(primary_description, &links(facts))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORGE: &str = "https://forge.corbet.ch";

    fn facts(provider: Provider, private: bool) -> Facts<'static> {
        Facts {
            provider,
            private,
            forge_origin: FORGE,
            source_path: "corbet-cid/ccid",
            issues: true,
            wiki: true,
            github: None,
        }
    }

    fn visible(issues: bool, wiki: bool) -> Option<PublicGithub<'static>> {
        Some(PublicGithub {
            path: "corbet-cid/ccid",
            issues,
            wiki,
        })
    }

    #[test]
    fn gitlab_shows_issues_and_wiki_itself_and_carries_the_primary_description_unchanged() {
        for private in [false, true] {
            let f = facts(Provider::Gitlab, private);
            assert!(links(&f).is_empty());
            assert_eq!(copy_description("A tool", &f), "A tool");
        }
    }

    #[test]
    fn a_public_github_copy_holds_the_wiki_and_a_private_one_links_to_forgejo() {
        let mut f = facts(Provider::Github, false);
        f.github = visible(true, true);
        assert_eq!(copy_description("A tool", &f), "A tool");
        let mut f = facts(Provider::Github, true);
        f.github = visible(true, true);
        assert_eq!(
            copy_description("A tool", &f),
            "A tool · Wiki: https://forge.corbet.ch/corbet-cid/ccid/wiki"
        );
    }

    #[test]
    fn github_without_wiki_pages_on_the_primary_carries_no_link() {
        let mut f = facts(Provider::Github, false);
        f.wiki = false;
        assert_eq!(copy_description("A tool", &f), "A tool");
    }

    #[test]
    fn public_bitbucket_links_issues_to_a_visible_github_copy_and_the_wiki_to_forgejo_today() {
        let mut f = facts(Provider::Bitbucket, false);
        f.github = visible(true, false);
        assert_eq!(
            copy_description("A tool", &f),
            "A tool · Issues: https://github.com/corbet-cid/ccid/issues · Wiki: https://forge.corbet.ch/corbet-cid/ccid/wiki"
        );
    }

    #[test]
    fn public_bitbucket_links_the_wiki_to_github_only_once_github_holds_it() {
        let mut f = facts(Provider::Bitbucket, false);
        f.github = visible(true, true);
        assert_eq!(links(&f)[1].url, "https://github.com/corbet-cid/ccid/wiki");
    }

    #[test]
    fn public_bitbucket_without_a_visible_github_copy_links_to_forgejo() {
        let f = facts(Provider::Bitbucket, false);
        assert_eq!(
            copy_description("", &f),
            "Issues: https://forge.corbet.ch/corbet-cid/ccid/issues · Wiki: https://forge.corbet.ch/corbet-cid/ccid/wiki"
        );
        let mut f = facts(Provider::Bitbucket, false);
        f.github = visible(false, false);
        assert_eq!(
            links(&f)[0].url,
            "https://forge.corbet.ch/corbet-cid/ccid/issues"
        );
    }

    #[test]
    fn private_bitbucket_always_links_to_forgejo_even_when_github_is_given() {
        let mut f = facts(Provider::Bitbucket, true);
        f.github = visible(true, true);
        let all = links(&f);
        assert!(all.iter().all(|l| l.url.starts_with(FORGE)), "{all:?}");
    }

    #[test]
    fn bitbucket_links_only_what_the_primary_has() {
        let mut f = facts(Provider::Bitbucket, false);
        f.wiki = false;
        assert_eq!(links(&f).len(), 1);
        f.issues = false;
        assert!(links(&f).is_empty());
        assert_eq!(copy_description("  A tool  ", &f), "A tool");
    }

    #[test]
    fn the_origin_may_end_with_a_slash() {
        let mut f = facts(Provider::Github, true);
        f.forge_origin = "https://forge.corbet.ch/";
        assert_eq!(
            links(&f)[0].url,
            "https://forge.corbet.ch/corbet-cid/ccid/wiki"
        );
    }

    #[test]
    fn the_result_is_stable_so_a_second_pass_plans_nothing() {
        let f = facts(Provider::Bitbucket, false);
        let once = copy_description("A tool", &f);
        assert_eq!(copy_description("A tool", &f), once);
    }
}
