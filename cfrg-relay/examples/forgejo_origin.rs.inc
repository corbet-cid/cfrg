struct ForgejoOrigin {
    rt: Rc<tokio::runtime::Runtime>,
    driver: cfgj::relay::ForgejoRelay,
}
impl ForgejoOrigin {
    fn repo(path: &str) -> Result<cfgj::relay::Repo, ForgeError> {
        cfrg_relay::live::forgejo_repo(path)
    }
}
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
        let pull = self
            .rt
            .block_on(self.driver.pull_read(&Self::repo(repo)?, n as i64))
            .map_err(cfrg_relay::live::forgejo_error)?;
        Ok(pull.map(|p| {
            if p.merged {
                ReqState::Merged
            } else if p.state == "closed" {
                ReqState::Closed
            } else {
                ReqState::Open
            }
        }))
    }
    fn close_request(&mut self, repo: &str, n: u64, note: &str) -> Result<(), ForgeError> {
        let r = Self::repo(repo)?;
        self.rt
            .block_on(self.driver.comment(&r, n as i64, note))
            .map_err(cfrg_relay::live::forgejo_error)?;
        self.rt
            .block_on(self.driver.pull_close(&r, n as i64))
            .map_err(cfrg_relay::live::forgejo_error)
    }
    fn delete_branch(&mut self, _: &Site, _: &str) -> Result<(), ForgeError> {
        Ok(())
    }
}
