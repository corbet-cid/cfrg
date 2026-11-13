//! Who may send us events: our own repositories only.
//!
//! The GitHub App `corbet-forge` must be installable by other accounts (GitHub only lets a private App be installed on
//! its owner), so anyone could install it and send signed events. Right after the signature check an event is dropped
//! (answer 202, one log line, no database work) unless it belongs to us:
//!
//! * GitHub: `installation.account.login` (when present) and the repository owner are organisations of the placement's
//!   GitHub copies;
//! * GitLab and Forgejo: the repository id is a member of the relay's repository set.
//!
//! The set is derived from the placement exactly as the relay derives it (`relay::derive`), contributed projects
//! excluded. An empty set drops everything.
use crate::config::Result;
use cfrg::{
    contributed::Contributed, model::Forge, native::Placement, relay::derive::repos_from_placement,
};
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::Path};

#[derive(Debug, Default, Clone)]
pub struct Allow {
    github_owners: BTreeSet<String>,
    ids: BTreeSet<(Forge, String)>,
}

impl Allow {
    pub fn load(path: &Path, extra_forge_hosts: &[&str]) -> Result<Self> {
        let placement = Placement::from_document(&fs::read(path)?)?;
        let contributed = Contributed::from_placement(&placement, extra_forge_hosts)?;
        let mut allow = Self::default();
        for set in repos_from_placement(&placement, &contributed) {
            for (forge, member) in set.members {
                if forge == Forge::Github {
                    if let Some(owner) = member.path.split('/').next() {
                        allow.github_owners.insert(owner.to_ascii_lowercase());
                    }
                }
                allow.ids.insert((forge, member.id));
            }
        }
        Ok(allow)
    }

    /// `true` when the event is about one of our repositories. `repository` is the id the event names.
    pub fn permits(&self, forge: Forge, repository: &str, payload: &Value) -> bool {
        match forge {
            Forge::Github => {
                let login = |pointer: &str| {
                    payload
                        .pointer(pointer)
                        .and_then(Value::as_str)
                        .map(str::to_ascii_lowercase)
                };
                let owner = login("/repository/owner/login").or_else(|| {
                    payload
                        .pointer("/repository/full_name")
                        .and_then(Value::as_str)
                        .and_then(|n| n.split('/').next())
                        .map(str::to_ascii_lowercase)
                });
                let ours = |l: &String| self.github_owners.contains(l);
                owner.as_ref().is_some_and(ours)
                    && login("/installation/account/login")
                        .as_ref()
                        .is_none_or(ours)
            }
            other => self.ids.contains(&(other, repository.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub fn placement_text() -> String {
        let dest = |provider: &str, host: &str, id: &str, path: &str, ns: &str| {
            format!(
                r#"{{"provider":"{provider}","endpoint":{{"origin":"https://{host}","token_env":"T"}},"path":"{path}","namespace":"{ns}","repository_id":"{id}",
                "mirror_user":"u","password_env":"T","use_ssh":false,"interval_seconds":3600,"branch_filter":"","hold":null,"absent":false,"remote_name":null}}"#
            )
        };
        format!(
            r#"{{"native":{{"schema":1,"source":{{"origin":"https://forge.corbet.ch","token_env":"T"}},"repositories":[
            {{"path":"corbet-fsl/ccvl","source_id":8,"private":false,"default_branch":"main","content":"profile","hold":null,"destinations":[{},{}]}}],
            "contributed":[]}}}}"#,
            dest(
                "github",
                "api.github.com",
                "4242",
                "corbet-fsl/ccvl",
                "corbet-fsl"
            ),
            dest("gitlab", "gitlab.com", "55", "corbet-fsl/ccvl", "2")
        )
    }

    #[test]
    fn only_our_organisations_and_repositories_pass() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("placement.json");
        fs::write(&file, placement_text()).unwrap();
        let allow = Allow::load(&file, &[]).unwrap();
        let ours = json!({"repository": {"id": 4242, "owner": {"login": "Corbet-FSL"}}, "installation": {"account": {"login": "corbet-fsl"}}});
        assert!(allow.permits(Forge::Github, "4242", &ours));
        let foreign_installation = json!({"repository": {"id": 4242, "owner": {"login": "corbet-fsl"}}, "installation": {"account": {"login": "mallory"}}});
        assert!(!allow.permits(Forge::Github, "4242", &foreign_installation));
        let foreign_repo = json!({"repository": {"id": 1, "full_name": "mallory/x"}, "installation": {"account": {"login": "corbet-fsl"}}});
        assert!(!allow.permits(Forge::Github, "1", &foreign_repo));
        assert!(!allow.permits(Forge::Github, "1", &json!({})));
        assert!(
            allow.permits(Forge::Gitlab, "55", &json!({}))
                && !allow.permits(Forge::Gitlab, "56", &json!({}))
        );
        assert!(
            allow.permits(Forge::Forgejo, "8", &json!({}))
                && !allow.permits(Forge::Forgejo, "9", &json!({}))
        );
        let nothing = Allow::default();
        assert!(!nothing.permits(Forge::Forgejo, "8", &json!({})));
    }
}
