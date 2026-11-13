//! RD9 markers, the R41 relayed-post format, and the text rewrites. All pure.
//!
//! A marker is a hidden HTML comment on a line of its own, in a footer block at the very end of a body:
//!
//! ```text
//! <!-- cforge:v1:<lid>:<forge> -->                                   RD9: this object is that logical object
//! <!-- cforge:v1:<lid>:link:<forge>:<repo>:<number>:<origin|copy> --> a sibling of the object
//! <!-- cforge:v1:<lcid>:from:<forge>:<comment id> -->                the origin of a copied comment
//! ```
//!
//! A marker is **trusted only when its object or comment was written by the relay identity** (RD9). A
//! relayed body is copied verbatim into a relay-authored object, so a marker an outsider pasted would
//! travel into trusted territory: [`neutralise`] breaks every `cforge:v1:` in a relayed text, and only
//! the footer block is ever parsed.

use super::model::{forge_title, Actor, Credit, Lid, RouterConfig};
use crate::model::Forge;

const PREFIX: &str = "<!-- cforge:v1:";
const SUFFIX: &str = " -->";
const ZWSP: char = '\u{200b}';

/// Marker fields are written with letters, digits, `_` and `.` only; anything else is hex-escaped
/// behind a `~`. No field can contain `:` or `-`, so a marker can neither be split nor end its comment early.
pub fn enc(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.');
    if plain {
        value.to_owned()
    } else {
        let mut out = String::from("~");
        for b in value.bytes() {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }
}

pub fn dec(field: &str) -> Option<String> {
    if let Some(hex) = field.strip_prefix('~') {
        if hex.is_empty() || hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let bytes: Option<Vec<u8>> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
            .collect();
        String::from_utf8(bytes?).ok()
    } else if !field.is_empty()
        && field
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
    {
        Some(field.to_owned())
    } else {
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Marker {
    Object {
        lid: Lid,
        forge: Forge,
    },
    Link {
        lid: Lid,
        forge: Forge,
        repo: String,
        number: u64,
        origin: bool,
    },
    From {
        lid: Lid,
        forge: Forge,
        id: u64,
    },
}

impl Marker {
    pub fn render(&self) -> String {
        let body = match self {
            Self::Object { lid, forge } => format!("{}:{}", enc(lid), forge.as_str()),
            Self::Link {
                lid,
                forge,
                repo,
                number,
                origin,
            } => format!(
                "{}:link:{}:{}:{}:{}",
                enc(lid),
                forge.as_str(),
                enc(repo),
                number,
                if *origin { "origin" } else { "copy" }
            ),
            Self::From { lid, forge, id } => {
                format!("{}:from:{}:{}", enc(lid), forge.as_str(), id)
            }
        };
        format!("{PREFIX}{body}{SUFFIX}")
    }

    pub fn parse_line(line: &str) -> Option<Self> {
        let inner = line.trim().strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
        let parts: Vec<&str> = inner.split(':').collect();
        let forge = |s: &str| Forge::parse(s).ok();
        match parts.as_slice() {
            [lid, f] => Some(Self::Object {
                lid: dec(lid)?,
                forge: forge(f)?,
            }),
            [lid, "link", f, repo, number, role] => Some(Self::Link {
                lid: dec(lid)?,
                forge: forge(f)?,
                repo: dec(repo)?,
                number: number.parse().ok()?,
                origin: match *role {
                    "origin" => true,
                    "copy" => false,
                    _ => return None,
                },
            }),
            [lid, "from", f, id] => Some(Self::From {
                lid: dec(lid)?,
                forge: forge(f)?,
                id: id.parse().ok()?,
            }),
            _ => None,
        }
    }
}

/// The markers of the footer block: the run of marker lines at the very end of the body (blank lines
/// after it are allowed). Marker-looking lines anywhere else are text, never markers.
pub fn footer_markers(body: &str) -> Vec<Marker> {
    let mut found = Vec::new();
    for line in body.lines().rev() {
        if line.trim().is_empty() && found.is_empty() {
            continue;
        }
        match Marker::parse_line(line) {
            Some(marker) => found.push(marker),
            None => break,
        }
    }
    found.reverse();
    found
}

/// RD9: markers count only on an object or comment authored by the relay identity.
pub fn trusted_markers(body: &str, author_is_relay: bool) -> Vec<Marker> {
    if author_is_relay {
        footer_markers(body)
    } else {
        Vec::new()
    }
}

/// Breaks every `cforge:v1:` in text that is about to be copied into a relay-authored post.
pub fn neutralise(text: &str) -> String {
    text.replace("cforge:v1:", &format!("cforge{ZWSP}:v1:"))
}

// --- relayed post format (R41) -------------------------------------------------------------------

/// A sibling as the header lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SibLink {
    pub forge: Forge,
    pub repo: String,
    pub number: u64,
    pub url: String,
    pub origin: bool,
}

/// Everything the rewrite of one relayed text depends on.
pub struct Rewrite<'a> {
    pub cfg: &'a RouterConfig,
    pub source: Forge,
    pub target: Forge,
    /// `#N` of the origin repository to the number of the sibling on the target, where one exists.
    pub issue_on_target: &'a dyn Fn(u64) -> Option<u64>,
    /// Link to `#N` on the origin forge.
    pub origin_issue_url: &'a dyn Fn(u64) -> String,
}

fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-' | '\''))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// First line of every relayed post: "**@user** wrote this on GitLab: <link>", or the plain name where
/// the identity table has no account on the target forge, or "Relayed from GitLab: <link>" when the
/// project opted out of highlighting.
pub fn credit_line(
    cfg: &RouterConfig,
    credit: Credit,
    source: Forge,
    target: Forge,
    author: &Actor,
    link: &str,
) -> String {
    match credit {
        Credit::Plain => format!("Relayed from {}: {link}", forge_title(source)),
        Credit::Highlight => {
            let person = cfg.person_by_handle(source, &author.login);
            let who = match person.and_then(|p| p.handles.get(&target)) {
                Some(handle) => format!("@{}", clean_name(handle).replace(' ', "")),
                None => {
                    let name = person.map_or_else(|| author.login.clone(), |p| p.name.clone());
                    let name = clean_name(&name);
                    if name.is_empty() {
                        "someone".into()
                    } else {
                        name
                    }
                }
            };
            format!("**{who}** wrote this on {}: {link}", forge_title(source))
        }
    }
}

fn is_handle_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// `@handle` mentions the identity table does not map are neutralised (the same handle on another
/// forge is a different person); `#N` references go to the sibling's number where it is known and to a
/// link to the origin otherwise. Code fences and inline code are left as they are.
pub fn rewrite_text(text: &str, rw: &Rewrite<'_>) -> String {
    let text = neutralise(text);
    let mut out = String::with_capacity(text.len() + 16);
    let mut fenced = false;
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            out.push_str(line);
            continue;
        }
        if fenced {
            out.push_str(line);
            continue;
        }
        for (j, segment) in line.split('`').enumerate() {
            if j > 0 {
                out.push('`');
            }
            if j % 2 == 0 {
                out.push_str(&rewrite_segment(segment, rw));
            } else {
                out.push_str(segment);
            }
        }
    }
    out
}

fn rewrite_segment(segment: &str, rw: &Rewrite<'_>) -> String {
    let chars: Vec<char> = segment.chars().collect();
    let mut out = String::with_capacity(segment.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let prev = if i == 0 { None } else { Some(chars[i - 1]) };
        if c == '@'
            && prev.is_none_or(|p| {
                !(p.is_alphanumeric() || matches!(p, '_' | '/' | '@' | '.' | '-' | '&'))
            })
        {
            let mut j = i + 1;
            while j < chars.len() && is_handle_char(chars[j]) {
                j += 1;
            }
            while j > i + 1 && matches!(chars[j - 1], '.' | '-') {
                j -= 1;
            }
            if j > i + 1 {
                let login: String = chars[i + 1..j].iter().collect();
                let mapped = rw
                    .cfg
                    .person_by_handle(rw.source, &login)
                    .and_then(|p| p.handles.get(&rw.target));
                match mapped {
                    Some(handle) => {
                        out.push('@');
                        out.push_str(&clean_name(handle).replace(' ', ""));
                    }
                    None => {
                        out.push('@');
                        out.push(ZWSP);
                        out.push_str(&login);
                    }
                }
                i = j;
                continue;
            }
        }
        if c == '#'
            && prev.is_none_or(|p| !(p.is_alphanumeric() || matches!(p, '_' | '&' | '/' | '#')))
        {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            let digits = j - (i + 1);
            let boundary = chars
                .get(j)
                .is_none_or(|n| !(n.is_alphanumeric() || *n == '_'));
            if (1..=9).contains(&digits) && boundary {
                let number: u64 = chars[i + 1..j]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0);
                match (rw.issue_on_target)(number) {
                    Some(mapped) => out.push_str(&format!("#{mapped}")),
                    None => {
                        out.push_str(&format!("[#{number}]({})", (rw.origin_issue_url)(number)))
                    }
                }
                i = j;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn sibling_markers(lid: &str, target: Forge, siblings: &[SibLink]) -> String {
    let mut lines = vec![Marker::Object {
        lid: lid.to_owned(),
        forge: target,
    }
    .render()];
    // The holder's own entry is left out: its Object marker already names it, and a body that does
    // not list itself stays correct while a sibling is added to the other bodies.
    for s in siblings.iter().filter(|s| s.forge != target) {
        lines.push(
            Marker::Link {
                lid: lid.to_owned(),
                forge: s.forge,
                repo: s.repo.clone(),
                number: s.number,
                origin: s.origin,
            }
            .render(),
        );
    }
    lines.join("\n")
}

fn visible_siblings(siblings: &[SibLink], except: Forge) -> String {
    let items: Vec<String> = siblings
        .iter()
        .filter(|s| s.forge != except)
        .map(|s| format!("[{} #{}]({})", forge_title(s.forge), s.number, s.url))
        .collect();
    if items.is_empty() {
        String::new()
    } else {
        format!("Also on: {}", items.join(" · "))
    }
}

/// The body of a copy of an object: credit, sibling list, the origin's text, markers.
pub fn render_object_body(
    credit_line: &str,
    lid: &str,
    target: Forge,
    siblings: &[SibLink],
    canonical_body: &str,
    rw: &Rewrite<'_>,
) -> String {
    let mut parts = vec![credit_line.to_owned()];
    let list = visible_siblings(siblings, target);
    if !list.is_empty() {
        parts.push(list);
    }
    let head = parts.join("\n");
    format!(
        "{head}\n\n---\n\n{}\n\n{}\n",
        rewrite_text(canonical_body, rw),
        sibling_markers(lid, target, siblings)
    )
}

/// The cforge comment on a human-authored object that lists the siblings (edited in place).
pub fn render_list_comment(lid: &str, on: Forge, siblings: &[SibLink]) -> String {
    let mut lines = vec!["This issue is also on:".to_owned(), String::new()];
    for s in siblings.iter().filter(|s| s.forge != on) {
        lines.push(format!(
            "- [{} #{}]({})",
            forge_title(s.forge),
            s.number,
            s.url
        ));
    }
    format!(
        "{}\n\n{}\n",
        lines.join("\n"),
        sibling_markers(lid, on, siblings)
    )
}

/// The body of a copy of a comment: credit, the origin's text, markers.
pub fn render_comment_body(
    credit_line: &str,
    lcid: &str,
    target: Forge,
    origin_forge: Forge,
    origin_id: u64,
    text: &str,
    rw: &Rewrite<'_>,
) -> String {
    format!(
        "{credit_line}\n\n{}\n\n{}\n{}\n",
        rewrite_text(text, rw),
        Marker::Object {
            lid: lcid.to_owned(),
            forge: target
        }
        .render(),
        Marker::From {
            lid: lcid.to_owned(),
            forge: origin_forge,
            id: origin_id
        }
        .render()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::model::Person;
    use std::collections::BTreeMap;

    fn cfg() -> RouterConfig {
        RouterConfig {
            people: vec![Person {
                key: "ada".into(),
                name: "Ada Lovelace".into(),
                handles: BTreeMap::from([
                    (Forge::Gitlab, "ada-gl".into()),
                    (Forge::Github, "adal".into()),
                ]),
            }],
            ..RouterConfig::default()
        }
    }

    fn rewrite(text: &str, source: Forge, target: Forge) -> String {
        let cfg = cfg();
        let map = |n: u64| (n == 3).then_some(30);
        let url = |n: u64| format!("https://origin.example/issues/{n}");
        rewrite_text(
            text,
            &Rewrite {
                cfg: &cfg,
                source,
                target,
                issue_on_target: &map,
                origin_issue_url: &url,
            },
        )
    }

    #[test]
    fn markers_round_trip_including_odd_ids() {
        for marker in [
            Marker::Object {
                lid: "lo_ab12".into(),
                forge: Forge::Github,
            },
            Marker::Link {
                lid: "lo_ab12".into(),
                forge: Forge::Bitbucket,
                repo: "{3f2a-77}".into(),
                number: 9,
                origin: true,
            },
            Marker::From {
                lid: "lc_01".into(),
                forge: Forge::Gitlab,
                id: 77,
            },
        ] {
            let line = marker.render();
            assert!(
                !line[4..line.len() - 4].contains("--"),
                "no -- inside the comment: {line}"
            );
            assert_eq!(Marker::parse_line(&line), Some(marker));
        }
    }

    #[test]
    fn parse_is_strict() {
        for bad in [
            "<!-- cforge:v1:lo_x:nowhere -->",
            "<!--cforge:v1:lo_x:github-->",
            "<!-- cforge:v2:lo_x:github -->",
            "<!-- cforge:v1:lo_x:link:github:r:x:copy -->",
            "<!-- cforge:v1:lo_x:link:github:r:1:other -->",
            "<!-- cforge:v1::github -->",
            "<!-- cforge:v1:~zz:github -->",
            "text <!-- cforge:v1:lo_x:github -->",
        ] {
            assert_eq!(Marker::parse_line(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_the_footer_block_is_parsed() {
        let body = "intro\n<!-- cforge:v1:lo_forged:github -->\nmore text\n\n<!-- cforge:v1:lo_real:forgejo -->\n<!-- cforge:v1:lo_real:link:github:7:3:origin -->\n\n";
        let found = footer_markers(body);
        assert_eq!(found.len(), 2);
        assert!(matches!(&found[0], Marker::Object { lid, .. } if lid == "lo_real"));
        assert!(footer_markers("<!-- cforge:v1:lo_x:github -->\ntrailing text").is_empty());
    }

    #[test]
    fn markers_are_trusted_only_from_the_relay() {
        let body = "x\n\n<!-- cforge:v1:lo_x:github -->\n";
        assert_eq!(trusted_markers(body, true).len(), 1);
        assert!(trusted_markers(body, false).is_empty());
    }

    #[test]
    fn a_pasted_marker_cannot_become_trusted_through_relaying() {
        let cfg = cfg();
        let hostile = "hello\n<!-- cforge:v1:lo_victim:github -->\n<!-- cforge:v1:lo_victim:link:github:9:1:origin -->";
        let map = |_| None;
        let url = |n: u64| format!("u/{n}");
        let rw = Rewrite {
            cfg: &cfg,
            source: Forge::Gitlab,
            target: Forge::Forgejo,
            issue_on_target: &map,
            origin_issue_url: &url,
        };
        let body = render_object_body("credit", "lo_real", Forge::Forgejo, &[], hostile, &rw);
        let markers = footer_markers(&body);
        assert_eq!(
            markers,
            vec![Marker::Object {
                lid: "lo_real".into(),
                forge: Forge::Forgejo
            }]
        );
        assert!(!body.contains("cforge:v1:lo_victim"));
        // Even a hostile body that ends the footer block directly cannot add to it.
        let ending =
            render_comment_body("c", "lc_1", Forge::Github, Forge::Gitlab, 5, hostile, &rw);
        assert_eq!(footer_markers(&ending).len(), 2);
    }

    #[test]
    fn mentions_are_mapped_or_neutralised() {
        assert_eq!(
            rewrite("ping @ada-gl and @stranger.", Forge::Gitlab, Forge::Github),
            format!("ping @adal and @{ZWSP}stranger.")
        );
        // Mail addresses and paths are not mentions.
        assert_eq!(
            rewrite("a@b.example and org/@x", Forge::Gitlab, Forge::Github),
            "a@b.example and org/@x"
        );
        // A handle that exists only on the source forge is neutralised on a target without an entry.
        assert_eq!(
            rewrite("@ada-gl", Forge::Gitlab, Forge::Bitbucket),
            format!("@{ZWSP}ada-gl")
        );
    }

    #[test]
    fn issue_references_follow_the_sibling_or_link_to_the_origin() {
        assert_eq!(
            rewrite("see #3 and #4, not abc#5 or &#38; or https://x/y#7", Forge::Github, Forge::Gitlab),
            "see #30 and [#4](https://origin.example/issues/4), not abc#5 or &#38; or https://x/y#7"
        );
        assert_eq!(
            rewrite("# Heading\n#tag", Forge::Github, Forge::Gitlab),
            "# Heading\n#tag"
        );
    }

    #[test]
    fn code_is_left_alone() {
        let text = "`@x #3` and @x\n```\n@x #3\n```\nafter #3";
        assert_eq!(
            rewrite(text, Forge::Github, Forge::Gitlab),
            format!("`@x #3` and @{ZWSP}x\n```\n@x #3\n```\nafter #30")
        );
    }

    #[test]
    fn credit_line_uses_the_mention_only_where_the_table_knows_the_account() {
        let cfg = cfg();
        let author = Actor {
            id: "1".into(),
            login: "ADA-GL".into(),
        };
        assert_eq!(
            credit_line(
                &cfg,
                Credit::Highlight,
                Forge::Gitlab,
                Forge::Github,
                &author,
                "https://l"
            ),
            "**@adal** wrote this on GitLab: https://l"
        );
        assert_eq!(
            credit_line(
                &cfg,
                Credit::Highlight,
                Forge::Gitlab,
                Forge::Forgejo,
                &author,
                "https://l"
            ),
            "**Ada Lovelace** wrote this on GitLab: https://l"
        );
        let stranger = Actor {
            id: "2".into(),
            login: "x*y_z".into(),
        };
        assert_eq!(
            credit_line(
                &cfg,
                Credit::Highlight,
                Forge::Github,
                Forge::Gitlab,
                &stranger,
                "https://l"
            ),
            "**xy_z** wrote this on GitHub: https://l"
        );
        assert_eq!(
            credit_line(
                &cfg,
                Credit::Plain,
                Forge::Gitlab,
                Forge::Github,
                &author,
                "https://l"
            ),
            "Relayed from GitLab: https://l"
        );
    }

    #[test]
    fn rendered_bodies_carry_the_sibling_list_and_markers() {
        let cfg = cfg();
        let map = |_| None;
        let url = |n: u64| format!("u/{n}");
        let rw = Rewrite {
            cfg: &cfg,
            source: Forge::Gitlab,
            target: Forge::Forgejo,
            issue_on_target: &map,
            origin_issue_url: &url,
        };
        let sibs = vec![
            SibLink {
                forge: Forge::Gitlab,
                repo: "42".into(),
                number: 5,
                url: "https://gl/5".into(),
                origin: true,
            },
            SibLink {
                forge: Forge::Forgejo,
                repo: "7".into(),
                number: 8,
                url: "https://fj/8".into(),
                origin: false,
            },
        ];
        let body = render_object_body("credit", "lo_1", Forge::Forgejo, &sibs, "text", &rw);
        assert!(body.starts_with("credit\nAlso on: [GitLab #5](https://gl/5)\n\n---\n\ntext\n"));
        let markers = footer_markers(&body);
        assert_eq!(
            markers.len(),
            2,
            "the object marker and the one other sibling"
        );
        let list = render_list_comment("lo_1", Forge::Gitlab, &sibs);
        assert!(list.contains("- [Forgejo #8](https://fj/8)"));
        assert!(!list.contains("- [GitLab"));
        assert_eq!(footer_markers(&list).len(), 2);
    }
}
