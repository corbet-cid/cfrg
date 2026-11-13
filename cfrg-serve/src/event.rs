//! From a verified delivery to the five fields the relay job gets (Bridge design 3.1): forge, repository, kind,
//! number, action. Nothing else of the payload is trusted or forwarded; the job re-reads the object from its forge.
use cfrg::model::Forge;
use cfrg::relay::{Hint, HintAction};
use http::HeaderMap;
use serde_json::Value;
use std::collections::BTreeMap;

/// The only things a dispatch carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatch {
    pub forge: Forge,
    /// The forge's immutable repository id (RF14), as text.
    pub repository: String,
    /// `issue`, `contribution`, or `wiki` (a Forgejo wiki changed; number 0, action `changed`; R40).
    pub kind: &'static str,
    pub number: u64,
    /// `changed`, `deleted` or `comment_deleted:<comment id>`.
    pub action: String,
}

impl Dispatch {
    /// The workflow inputs: exactly five strings.
    pub fn inputs(&self) -> BTreeMap<&'static str, String> {
        BTreeMap::from([
            ("forge", self.forge.as_str().to_owned()),
            ("repository", self.repository.clone()),
            ("kind", self.kind.to_owned()),
            ("number", self.number.to_string()),
            ("action", self.action.clone()),
        ])
    }

    /// The hint `cfrg relay apply` reads: the dispatch in the relay's own type, so the two cannot drift apart.
    pub fn hint(&self) -> Result<Hint, String> {
        if self.kind == "wiki" {
            return Err("a wiki event is not a relay hint".into());
        }
        let action = match self.action.as_str() {
            "changed" => HintAction::Changed,
            "deleted" => HintAction::Deleted,
            other => match other
                .strip_prefix("comment_deleted:")
                .and_then(|id| id.parse().ok())
            {
                Some(comment_id) => HintAction::CommentDeleted { comment_id },
                None => return Err(format!("unknown action {other:?}")),
            },
        };
        serde_json::from_value(serde_json::json!({
            "forge": self.forge.as_str(),
            "repo": self.repository,
            "kind": self.kind,
            "number": self.number,
            "action": serde_json::to_value(action).map_err(|e| e.to_string())?,
        }))
        .map_err(|e| e.to_string())
    }

    /// The key that makes two deliveries of one change the same job in the pending set.
    pub fn key(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.forge.as_str(),
            self.repository,
            self.kind,
            self.number
        )
    }
}

fn header<'a>(headers: &'a HeaderMap, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|n| headers.get(*n).and_then(|v| v.to_str().ok()))
}

fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => n.as_u64().map(|n| n.to_string()),
        Value::String(s)
            if !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) =>
        {
            Some(s.clone())
        }
        _ => None,
    }
}

fn number_of(value: &Value) -> Option<u64> {
    value.as_u64().filter(|n| *n > 0)
}

fn kind_name(pull: bool) -> &'static str {
    if pull {
        "contribution"
    } else {
        "issue"
    }
}

/// GitHub and Forgejo share the shape of issues, comments and pull requests.
fn github_like(forge: Forge, event: &str, payload: &Value) -> Option<Dispatch> {
    let repository = id_text(payload.pointer("/repository/id")?)?;
    let action = payload.get("action").and_then(Value::as_str).unwrap_or("");
    let (kind, number, action) = match event {
        "issues" => {
            let number = number_of(payload.pointer("/issue/number")?)?;
            let pull = !payload
                .pointer("/issue/pull_request")
                .is_none_or(Value::is_null);
            let action = if action == "deleted" {
                "deleted"
            } else {
                "changed"
            };
            (kind_name(pull), number, action.to_owned())
        }
        "issue_comment" => {
            let number = number_of(payload.pointer("/issue/number")?)?;
            let pull = !payload
                .pointer("/issue/pull_request")
                .is_none_or(Value::is_null);
            let action = if action == "deleted" {
                format!(
                    "comment_deleted:{}",
                    number_of(payload.pointer("/comment/id")?)?
                )
            } else {
                "changed".to_owned()
            };
            (kind_name(pull), number, action)
        }
        "pull_request" => {
            let number = number_of(
                payload
                    .get("number")
                    .or_else(|| payload.pointer("/pull_request/number"))?,
            )?;
            ("contribution", number, "changed".to_owned())
        }
        _ => return None,
    };
    Some(Dispatch {
        forge,
        repository,
        kind,
        number,
        action,
    })
}

pub fn github(headers: &HeaderMap, payload: &Value) -> Option<Dispatch> {
    github_like(
        Forge::Github,
        header(headers, &["x-github-event"])?,
        payload,
    )
}

pub fn forgejo(headers: &HeaderMap, payload: &Value) -> Option<Dispatch> {
    let event = header(headers, &["x-forgejo-event", "x-gitea-event"])?;
    if event == "wiki" {
        // Any page created, edited or deleted: the wiki leg re-reads the wiki head, so only the repository matters.
        return Some(Dispatch {
            forge: Forge::Forgejo,
            repository: id_text(payload.pointer("/repository/id")?)?,
            kind: "wiki",
            number: 0,
            action: "changed".to_owned(),
        });
    }
    github_like(Forge::Forgejo, event, payload)
}

/// GitLab: `object_kind` in the body (the header `X-Gitlab-Event` carries the same word as a title).
pub fn gitlab(_headers: &HeaderMap, payload: &Value) -> Option<Dispatch> {
    let repository = id_text(
        payload
            .pointer("/project/id")
            .or_else(|| payload.pointer("/project_id"))?,
    )?;
    let (kind, number) = match payload.get("object_kind")?.as_str()? {
        "issue" => (
            "issue",
            number_of(payload.pointer("/object_attributes/iid")?)?,
        ),
        "merge_request" => (
            "contribution",
            number_of(payload.pointer("/object_attributes/iid")?)?,
        ),
        "note" => match payload
            .pointer("/object_attributes/noteable_type")?
            .as_str()?
        {
            "Issue" => ("issue", number_of(payload.pointer("/issue/iid")?)?),
            "MergeRequest" => (
                "contribution",
                number_of(payload.pointer("/merge_request/iid")?)?,
            ),
            _ => return None,
        },
        _ => return None,
    };
    // GitLab sends no deletion events for issues, merge requests or notes: every delivery is a change.
    Some(Dispatch {
        forge: Forge::Gitlab,
        repository,
        kind,
        number,
        action: "changed".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use serde_json::json;

    fn with(name: &'static str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn github_issue_comment_and_pull_request() {
        let issue = json!({"action":"opened","repository":{"id":42},"issue":{"number":7},"sender":{"login":"x"},"title":"secret"});
        let d = github(&with("x-github-event", "issues"), &issue).unwrap();
        assert_eq!(
            (d.kind, d.number, d.action.as_str(), d.repository.as_str()),
            ("issue", 7, "changed", "42")
        );
        assert_eq!(
            d.inputs().len(),
            5,
            "the dispatch carries nothing but the five fields"
        );
        let deleted = json!({"action":"deleted","repository":{"id":42},"issue":{"number":7}});
        assert_eq!(
            github(&with("x-github-event", "issues"), &deleted)
                .unwrap()
                .action,
            "deleted"
        );
        let comment = json!({"action":"deleted","repository":{"id":42},"issue":{"number":7,"pull_request":{"url":"x"}},"comment":{"id":99}});
        let d = github(&with("x-github-event", "issue_comment"), &comment).unwrap();
        assert_eq!(
            (d.kind, d.action.as_str()),
            ("contribution", "comment_deleted:99")
        );
        let pull = json!({"action":"opened","repository":{"id":42},"number":3,"pull_request":{"number":3}});
        let d = github(&with("x-github-event", "pull_request"), &pull).unwrap();
        assert_eq!((d.kind, d.number), ("contribution", 3));
        assert!(github(&with("x-github-event", "push"), &pull).is_none());
        assert!(github(&HeaderMap::new(), &pull).is_none());
    }

    #[test]
    fn forgejo_wiki_hook_is_a_wiki_dispatch_and_only_there() {
        let wiki =
            json!({"action":"edited","repository":{"id":9},"page":"Home","sender":{"login":"x"}});
        let d = forgejo(&with("x-forgejo-event", "wiki"), &wiki).unwrap();
        assert_eq!(
            (d.kind, d.number, d.action.as_str(), d.repository.as_str()),
            ("wiki", 0, "changed", "9")
        );
        assert_eq!(d.inputs().len(), 5);
        assert!(d.hint().is_err());
        assert!(github(&with("x-github-event", "wiki"), &wiki).is_none());
    }

    #[test]
    fn gitlab_events() {
        let issue = json!({"object_kind":"issue","project":{"id":5},"object_attributes":{"iid":2}});
        let d = gitlab(&HeaderMap::new(), &issue).unwrap();
        assert_eq!(
            (d.forge, d.kind, d.number, d.repository.as_str()),
            (Forge::Gitlab, "issue", 2, "5")
        );
        let note = json!({"object_kind":"note","project":{"id":5},"object_attributes":{"noteable_type":"MergeRequest"},"merge_request":{"iid":4}});
        let d = gitlab(&HeaderMap::new(), &note).unwrap();
        assert_eq!((d.kind, d.number), ("contribution", 4));
        assert!(gitlab(
            &HeaderMap::new(),
            &json!({"object_kind":"push","project":{"id":5}})
        )
        .is_none());
    }

    #[test]
    fn forgejo_events_and_the_relay_hint() {
        let comment = json!({"action":"deleted","repository":{"id":9},"issue":{"number":1},"comment":{"id":12}});
        let d = forgejo(&with("x-forgejo-event", "issue_comment"), &comment).unwrap();
        let hint = d.hint().unwrap();
        assert_eq!(hint.number, 1);
        assert_eq!(hint.action, HintAction::CommentDeleted { comment_id: 12 });
        let changed = Dispatch {
            forge: Forge::Github,
            repository: "1".into(),
            kind: "issue",
            number: 2,
            action: "changed".into(),
        };
        assert_eq!(changed.hint().unwrap().action, HintAction::Changed);
        let odd = Dispatch {
            action: "reboot".into(),
            ..changed
        };
        assert!(odd.hint().is_err());
    }

    #[test]
    fn hostile_ids_are_not_forwarded() {
        let payload = json!({"action":"opened","repository":{"id":"a b; rm"},"issue":{"number":1}});
        assert!(github(&with("x-github-event", "issues"), &payload).is_none());
    }
}
