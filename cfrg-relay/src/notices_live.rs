//! The live Forgejo [`NoticeHost`]: contribution notices through the cfgj driver (typed calls only).
use crate::live::{forgejo_error, forgejo_repo};
use cfrg::relay::{
    forge::ForgeError,
    merge::Patch,
    notice::{ContributionMarker, NoticeHost, NoticeRef},
};
use std::rc::Rc;
use tokio::runtime::Runtime;

pub struct ForgejoNotices {
    pub driver: cfgj::relay::ForgejoRelay,
    pub rt: Rc<Runtime>,
}

fn number(n: i64) -> Result<u64, ForgeError> {
    u64::try_from(n).map_err(|_| ForgeError::Rejected("negative notice number".into()))
}

impl NoticeHost for ForgejoNotices {
    fn find(
        &mut self,
        repo: &str,
        origin: &str,
        reference: &str,
    ) -> Result<Option<NoticeRef>, ForgeError> {
        let repo = forgejo_repo(repo)?;
        let recent = self
            .rt
            .block_on(self.driver.issues_recent(&repo))
            .map_err(forgejo_error)?;
        Ok(recent.into_iter().find_map(|issue| {
            let labelled = issue.snap.labels.iter().any(|l| l == "contribution");
            ContributionMarker::parse(&issue.snap.body)
                .filter(|m| labelled && m.origin == origin && m.reference == reference)
                .map(|_| NoticeRef {
                    number: issue.number,
                    body: issue.snap.body,
                })
        }))
    }

    fn create(&mut self, repo: &str, title: &str, body: &str) -> Result<u64, ForgeError> {
        let repo = forgejo_repo(repo)?;
        self.rt
            .block_on(
                self.driver
                    .labels_ensure(&repo, &["contribution".to_owned()]),
            )
            .map_err(forgejo_error)?;
        let created = self
            .rt
            .block_on(self.driver.notice_create(&repo, title, body))
            .map_err(forgejo_error)?;
        number(created.number)
    }

    fn set_body(&mut self, repo: &str, n: u64, body: &str) -> Result<(), ForgeError> {
        let repo = forgejo_repo(repo)?;
        let patch = Patch {
            body: Some(body.to_owned()),
            ..Patch::default()
        };
        let n = i64::try_from(n).map_err(|_| ForgeError::Rejected("notice number".into()))?;
        self.rt
            .block_on(self.driver.issue_edit(&repo, n, &patch))
            .map_err(forgejo_error)
    }
}
