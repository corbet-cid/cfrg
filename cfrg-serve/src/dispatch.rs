//! The Forgejo `workflow_dispatch` of `relay.yml` in the infra repository.
//!
//! The credential is a Forgejo token limited to that one repository (`write:repository`), the narrowest that can
//! start a workflow. The request carries the five fields of [`Dispatch`] and nothing else. One attempt, no retry
//! (R30): a failure is logged and the delivery id is released so the sender's redelivery is accepted.
use crate::{config::DispatchConfig, event::Dispatch};
use reqwest::Client;
use std::time::Duration;

pub struct Dispatcher {
    client: Client,
    url: String,
    token: String,
    git_ref: String,
}

impl Dispatcher {
    pub fn new(client: Client, forge_api: &str, config: &DispatchConfig, token: &str) -> Self {
        let url = format!(
            "{}/repos/{}/actions/workflows/{}/dispatches",
            forge_api.trim_end_matches('/'),
            config.repository,
            config.workflow
        );
        Self {
            client,
            url,
            token: token.to_owned(),
            git_ref: config.git_ref.clone(),
        }
    }

    pub async fn run(&self, dispatch: &Dispatch) -> Result<(), String> {
        let body = serde_json::json!({"ref": self.git_ref, "inputs": dispatch.inputs()});
        let response = self
            .client
            .post(&self.url)
            .header("Authorization", format!("token {}", self.token))
            .timeout(Duration::from_secs(15))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("dispatch request failed: {e}"))?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("Forgejo answered HTTP {status} to the dispatch"))
        }
    }
}
