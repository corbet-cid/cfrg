struct ForgejoOrigin(Rest);
impl HeadHost for ForgejoOrigin {
    fn forge(&self) -> Forge {
        Forge::Forgejo
    }
    fn site(&mut self, _: &str) -> Result<Site, ForgeError> {
        Err(ForgeError::Rejected("origin only".into()))
    }
    fn branch_tip(&mut self, _: &Site, _: &str) -> Result<Option<String>, ForgeError> {
        Ok(None)
    }
    fn find_request(&mut self, _: &str, _: &Site, _: &str) -> Result<Option<ReqRef>, ForgeError> {
        Ok(None)
    }
    fn open_request(
        &mut self,
        _: &str,
        _: &Site,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<ReqRef, ForgeError> {
        Err(ForgeError::Rejected("origin only".into()))
    }
    fn request_state(&mut self, repo: &str, n: u64) -> Result<Option<ReqState>, ForgeError> {
        let v = self
            .0
            .call_opt("GET", &format!("repos/{repo}/pulls/{n}"), &[])?;
        Ok(v.map(|v| {
            if v["merged"].as_bool() == Some(true) {
                ReqState::Merged
            } else if v["state"].as_str() == Some("closed") {
                ReqState::Closed
            } else {
                ReqState::Open
            }
        }))
    }
    fn close_request(&mut self, repo: &str, n: u64, note: &str) -> Result<(), ForgeError> {
        self.0.call_opt(
            "POST",
            &format!("repos/{repo}/issues/{n}/comments"),
            &[("body", note)],
        )?;
        self.0.call_opt(
            "PATCH",
            &format!("repos/{repo}/pulls/{n}"),
            &[("state", "closed")],
        )?;
        Ok(())
    }
    fn delete_branch(&mut self, _: &Site, _: &str) -> Result<(), ForgeError> {
        Ok(())
    }
}

