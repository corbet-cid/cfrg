use super::*;
use std::cell::{Cell, RefCell};
use std::process::Command;

fn config() -> Config {
    Config {
        schema: 1,
        gitlab: "https://gitlab.example".into(),
        gitlab_project: 42,
        gitlab_repository: "team/project".into(),
        forgejo: "https://forge.example".into(),
        primary_repository: "team/project".into(),
        import_repository: "bridge/project".into(),
        target_branch: "main".into(),
    }
}

fn mr_fixture() -> MergeRequest {
    MergeRequest {
        iid: 7,
        project_id: 42,
        target_project_id: 42,
        state: "opened".into(),
        title: "A contribution".into(),
        sha: "a".repeat(40),
        target_branch: "main".into(),
        author: Author {
            id: 8,
            username: "contributor".into(),
        },
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum GitlabCall {
    MergeRequest,
    OpenList,
    Notes,
    PostNote,
    Close,
}

#[derive(Default)]
struct FakeGitlab {
    mr: RefCell<MergeRequest>,
    project_private: bool,
    refs: Vec<u64>,
    notes: RefCell<Vec<GitlabNote>>,
    posted_bodies: RefCell<Vec<String>>,
    actor: u64,
    lose_note: bool,
    lose_close: bool,
    mr_reads: Cell<usize>,
    moved_sha: bool,
    calls: RefCell<Vec<GitlabCall>>,
}

impl FakeGitlab {
    fn live() -> Self {
        FakeGitlab {
            mr: RefCell::new(mr_fixture()),
            actor: 99,
            ..Default::default()
        }
    }

    fn current_mr(&self) -> MergeRequest {
        self.mr_reads.set(self.mr_reads.get() + 1);
        let mut mr = self.mr.borrow().clone();
        if self.moved_sha && self.mr_reads.get() > 1 {
            mr.sha = "b".repeat(40);
        }
        mr
    }
}

impl GitlabBridge for FakeGitlab {
    fn mr_project(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _project: u64,
    ) -> Result<GitlabProject> {
        Ok(GitlabProject {
            id: 42,
            path_with_namespace: "team/project".into(),
            visibility: if self.project_private {
                "private".into()
            } else {
                "public".into()
            },
        })
    }

    fn merge_request(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _project: u64,
        _iid: u64,
    ) -> Result<MergeRequest> {
        self.calls.borrow_mut().push(GitlabCall::MergeRequest);
        Ok(self.current_mr())
    }

    fn open_merge_requests(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _project: u64,
    ) -> Result<Vec<MergeRequestRef>> {
        self.calls.borrow_mut().push(GitlabCall::OpenList);
        Ok(self
            .refs
            .iter()
            .map(|iid| MergeRequestRef { iid: *iid })
            .collect())
    }

    fn merge_request_notes(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _project: u64,
        _iid: u64,
    ) -> Result<Vec<GitlabNote>> {
        self.calls.borrow_mut().push(GitlabCall::Notes);
        Ok(self.notes.borrow().clone())
    }

    fn post_note(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _project: u64,
        _iid: u64,
        body: &str,
    ) -> Result<GitlabNote> {
        self.calls.borrow_mut().push(GitlabCall::PostNote);
        let note = GitlabNote {
            id: Some(100),
            body: Some(body.into()),
            author_id: Some(self.actor),
        };
        self.posted_bodies.borrow_mut().push(body.into());
        self.notes.borrow_mut().push(note.clone());
        if self.lose_note {
            return Err(failure("Lost note response"));
        }
        Ok(note)
    }

    fn close_merge_request(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _project: u64,
        _iid: u64,
    ) -> Result<()> {
        self.calls.borrow_mut().push(GitlabCall::Close);
        self.mr.borrow_mut().state = "closed".into();
        if self.lose_close {
            return Err(failure("Lost close response"));
        }
        Ok(())
    }

    fn current_user_id(&self, _transport: &mut dyn BridgeTransport, _origin: &str) -> Result<u64> {
        Ok(self.actor)
    }

    fn git_source(&self, origin: &str, repository: &str, iid: u64) -> GitSource {
        GitSource {
            url: format!("{origin}/{repository}.git"),
            token_env: Some("CFRG_BRIDGE_GITLAB_TOKEN"),
            username: "oauth2".into(),
            mr_ref: format!("refs/merge-requests/{iid}/head"),
        }
    }

    fn policy(&self, _origin: &str, _project: u64) -> Box<dyn MutationPolicy> {
        Box::new(AllowAll)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum ForgejoCall {
    Pulls,
    Pull,
    Create,
    Close,
}

#[derive(Default)]
struct FakeForgejo {
    pulls: RefCell<Vec<PullRequest>>,
    posts: Cell<usize>,
    review: Option<PullReview>,
    permission: String,
    actions: bool,
    hooks: bool,
    reject_create: bool,
    lose_create: bool,
    calls: RefCell<Vec<ForgejoCall>>,
}

impl ForgejoBridge for FakeForgejo {
    fn repository(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        path: &str,
    ) -> Result<ForgejoRepo> {
        let (id, full_name, fork, parent_id, owner_id) = match path {
            "team/project" => (10, "team/project", false, None, 12),
            "bridge/project" => (11, "bridge/project", true, Some(10), 12),
            _ => return Err(failure("Unknown test repository")),
        };
        Ok(ForgejoRepo {
            id,
            full_name: full_name.into(),
            private: false,
            has_actions: self.actions && !fork,
            fork,
            parent_id,
            owner_id,
        })
    }

    fn current_user_id(&self, _transport: &mut dyn BridgeTransport, _origin: &str) -> Result<u64> {
        Ok(12)
    }

    fn webhooks_empty(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _path: &str,
    ) -> Result<bool> {
        Ok(!self.hooks)
    }

    fn pulls(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _repository: &str,
    ) -> Result<Vec<PullRequest>> {
        self.calls.borrow_mut().push(ForgejoCall::Pulls);
        Ok(self.pulls.borrow().clone())
    }

    fn pull(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _repository: &str,
        index: u64,
    ) -> Result<PullRequest> {
        self.calls.borrow_mut().push(ForgejoCall::Pull);
        self.pulls
            .borrow()
            .iter()
            .find(|pull| pull.number == index)
            .cloned()
            .ok_or_else(|| failure("Unknown test pull"))
    }

    fn create_pull(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _repository: &str,
        head: &str,
        base: &str,
        _title: &str,
        body: &str,
    ) -> Result<PullRequest> {
        self.calls.borrow_mut().push(ForgejoCall::Create);
        self.posts.set(self.posts.get() + 1);
        if self.reject_create {
            return Err(failure("HTTP 503"));
        }
        let branch = head.strip_prefix("bridge:").unwrap_or(head).to_owned();
        let pull = PullRequest {
            number: 20 + self.pulls.borrow().len() as u64,
            state: "open".into(),
            merged: Some(false),
            head_ref: branch,
            head_repo_id: 11,
            head_sha: "a".repeat(40),
            base_ref: base.into(),
            base_repo_id: 10,
            base_sha: "b".repeat(40),
            user_id: 12,
            body: Some(body.into()),
        };
        self.pulls.borrow_mut().push(pull.clone());
        if self.lose_create {
            return Err(failure("Lost HTTP response"));
        }
        Ok(pull)
    }

    fn close_pull(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _repository: &str,
        index: u64,
        body: &str,
    ) -> Result<PullRequest> {
        self.calls.borrow_mut().push(ForgejoCall::Close);
        let mut live = self.pulls.borrow_mut();
        let pull = live
            .iter_mut()
            .find(|pull| pull.number == index)
            .ok_or_else(|| failure("Unknown test pull"))?;
        pull.state = "closed".into();
        pull.body = Some(body.into());
        Ok(pull.clone())
    }

    fn pull_review(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _repository: &str,
        _index: u64,
        _review: u64,
    ) -> Result<PullReview> {
        self.review.clone().ok_or_else(|| failure("No test review"))
    }

    fn collaborator_permission(
        &self,
        _transport: &mut dyn BridgeTransport,
        _origin: &str,
        _repository: &str,
        _user: &str,
    ) -> Result<CollaboratorPermission> {
        Ok(CollaboratorPermission {
            user_id: 50,
            permission: self.permission.clone(),
        })
    }

    fn git_dest(&self, origin: &str, repository: &str) -> GitDest {
        GitDest {
            url: format!("{origin}/{repository}.git"),
            token_env: Some("CFRG_BRIDGE_FORGEJO_TOKEN"),
            username: repository.split('/').next().unwrap_or_default().into(),
        }
    }

    fn policy(&self, _origin: &str, _primary: &str) -> Box<dyn MutationPolicy> {
        Box::new(AllowAll)
    }
}

struct AllowAll;

impl MutationPolicy for AllowAll {
    fn authorize(&self, _request: &BridgeRequest) -> Result<()> {
        Ok(())
    }
}

struct FakeTransport;
impl BridgeTransport for FakeTransport {
    fn execute(&mut self, _request: &BridgeRequest) -> Result<Value> {
        panic!("orchestrator tests never touch raw transport")
    }
}

#[derive(Default)]
struct FakeObjects {
    heads: Vec<String>,
    fail: bool,
}
impl Objects for FakeObjects {
    fn publish(
        &mut self,
        _source: &GitSource,
        _dest: &GitDest,
        sha: &str,
        branch: &str,
    ) -> Result<()> {
        assert!(branch.starts_with("ccid-import/gitlab/"));
        self.heads.push(sha.into());
        if self.fail {
            Err(failure("Git transport refused"))
        } else {
            Ok(())
        }
    }
}

fn review_fixture(message: String, head: String) -> PullReview {
    PullReview {
        id: 1,
        user_id: 50,
        login: "maintainer".into(),
        state: "APPROVED".into(),
        dismissed: false,
        stale: false,
        commit_id: head,
        body: message,
    }
}

#[test]
fn isolated_ci_requires_live_exact_maintainer_approval_and_removes_credentials() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let gitlab = FakeGitlab::live();
    let mut forgejo = FakeForgejo::default();
    let mut transport = FakeTransport;
    let mut objects = FakeObjects::default();
    import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
    )
    .unwrap();
    forgejo.pulls.borrow_mut()[0].head_sha = "a".repeat(40);
    forgejo.pulls.borrow_mut()[0].base_sha = "b".repeat(40);
    let head = "a".repeat(40);
    let base = "b".repeat(40);
    let request = json!({"head":head,"base":base,"review":1,"maintainer":"maintainer","maintainer_id":50,
        "sandbox":{"namespace":"ccid-untrusted-test","image":format!("example/image@sha256:{}","c".repeat(64)),"archive_sha256":"d".repeat(64),"command":["sh",".ci/test.sh"]}});
    let bytes = serde_json::to_vec(&request).unwrap();
    let message = approval::message(&bytes).unwrap();
    forgejo.review = Some(review_fixture(message.clone(), head.clone()));
    forgejo.permission = "write".into();
    let job = approval::plan(
        &gitlab,
        &forgejo,
        &mut transport,
        &config(),
        7,
        &journal,
        &bytes,
    )
    .unwrap();
    let pod = &job["spec"]["template"]["spec"];
    assert_eq!(pod["automountServiceAccountToken"], false);
    assert_eq!(
        pod["containers"][0]["securityContext"]["readOnlyRootFilesystem"],
        true
    );
    assert_eq!(pod["volumes"].as_array().unwrap().len(), 2);
    assert!(pod["volumes"][0]["hostPath"].is_null());
    assert_eq!(pod["containers"][0]["volumeMounts"][0]["readOnly"], true);
    assert!(pod["containers"][0]["envFrom"].is_null());
    forgejo.permission = "read".into();
    assert!(approval::plan(
        &gitlab,
        &forgejo,
        &mut transport,
        &config(),
        7,
        &journal,
        &bytes
    )
    .is_err());
    forgejo.permission = "write".into();
    forgejo.review.as_mut().unwrap().dismissed = true;
    assert!(approval::plan(
        &gitlab,
        &forgejo,
        &mut transport,
        &config(),
        7,
        &journal,
        &bytes
    )
    .is_err());
    forgejo.review.as_mut().unwrap().dismissed = false;
    forgejo.pulls.borrow_mut()[0].base_sha = "e".repeat(40);
    assert!(approval::plan(
        &gitlab,
        &forgejo,
        &mut transport,
        &config(),
        7,
        &journal,
        &bytes
    )
    .is_err());
}

#[test]
fn closure_recovers_lost_comment_and_close_responses_without_duplicates() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let mut gitlab = FakeGitlab::live();
    let forgejo = FakeForgejo::default();
    let mut transport = FakeTransport;
    let mut objects = FakeObjects::default();
    import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
    )
    .unwrap();
    forgejo.pulls.borrow_mut()[0].state = "closed".into();
    forgejo.pulls.borrow_mut()[0].merged = Some(true);
    gitlab.lose_note = true;
    assert!(
        lifecycle::feedback(&gitlab, &forgejo, &mut transport, &config(), 7, &journal).is_err()
    );
    assert_eq!(gitlab.notes.borrow().len(), 1);
    assert_eq!(gitlab.mr.borrow().state, "opened");
    gitlab.lose_note = false;
    gitlab.lose_close = true;
    assert!(
        lifecycle::feedback(&gitlab, &forgejo, &mut transport, &config(), 7, &journal).is_err()
    );
    assert_eq!(gitlab.mr.borrow().state, "closed");
    gitlab.lose_close = false;
    let result =
        lifecycle::feedback(&gitlab, &forgejo, &mut transport, &config(), 7, &journal).unwrap();
    assert_eq!(result["status"], "source_closed");
    assert_eq!(gitlab.notes.borrow().len(), 1);
    assert_eq!(gitlab.posted_bodies.borrow().len(), 1);
    assert!(forgejo.calls.borrow().contains(&ForgejoCall::Pull));
    assert!(gitlab.notes.borrow()[0]
        .body
        .as_deref()
        .unwrap()
        .contains("https://forge.example/team/project/pulls/20"));
}

#[test]
fn closure_refuses_to_discard_new_source_work() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let gitlab = FakeGitlab::live();
    let forgejo = FakeForgejo::default();
    let mut transport = FakeTransport;
    let mut objects = FakeObjects::default();
    import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
    )
    .unwrap();
    forgejo.pulls.borrow_mut()[0].state = "closed".into();
    gitlab.mr.borrow_mut().sha = "b".repeat(40);
    assert!(
        lifecycle::feedback(&gitlab, &forgejo, &mut transport, &config(), 7, &journal).is_err()
    );
    assert!(gitlab.notes.borrow().is_empty());
    assert_eq!(gitlab.mr.borrow().state, "opened");
}

#[test]
fn replacement_creates_new_generation_and_preserves_history_on_retry() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let gitlab = FakeGitlab::live();
    let forgejo = FakeForgejo::default();
    let mut transport = FakeTransport;
    let mut objects = FakeObjects::default();
    import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
    )
    .unwrap();
    let previous = forgejo.pulls.borrow_mut()[0].head_ref.clone();
    let head = "b".repeat(40);
    gitlab.mr.borrow_mut().sha = head.clone();
    let result = lifecycle::replace(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
        &head,
    )
    .unwrap();
    assert_eq!(result["pull_number"], 21);
    assert_eq!(forgejo.pulls.borrow_mut()[0].state, "closed");
    assert_eq!(forgejo.pulls.borrow_mut()[0].head_ref, previous);
    assert_eq!(forgejo.pulls.borrow_mut()[1].state, "open");
    lifecycle::replace(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
        &head,
    )
    .unwrap();
    assert_eq!(forgejo.posts.get(), 2);
    assert_eq!(
        import(
            &gitlab,
            &forgejo,
            &mut transport,
            &mut objects,
            &config(),
            7,
            &journal
        )
        .unwrap()["pull_number"],
        21
    );
}

#[test]
fn import_credits_author_and_is_idempotent_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let gitlab = FakeGitlab::live();
    let forgejo = FakeForgejo::default();
    let mut transport = FakeTransport;
    let mut objects = FakeObjects::default();
    {
        let journal = Journal::open(root.path()).unwrap();
        let report = import(
            &gitlab,
            &forgejo,
            &mut transport,
            &mut objects,
            &config(),
            7,
            &journal,
        )
        .unwrap();
        assert_eq!(report["pull_number"], 20);
        let body = forgejo.pulls.borrow_mut()[0].body.clone().unwrap();
        assert!(body.contains("Original author: GitLab `contributor` (user ID 8)"));
        assert!(body.contains("https://gitlab.example/team/project/-/merge_requests/7"));
        assert_eq!(report["ci"], "not_dispatched");
    }
    let journal = Journal::open(root.path()).unwrap();
    import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
    )
    .unwrap();
    assert_eq!(forgejo.posts.get(), 1);
    assert!(forgejo.calls.borrow().contains(&ForgejoCall::Pulls));
    assert!(forgejo.calls.borrow().contains(&ForgejoCall::Create));
    assert!(gitlab
        .calls
        .borrow()
        .iter()
        .all(|call| !matches!(call, GitlabCall::PostNote | GitlabCall::Close)));
}

#[test]
fn lost_post_response_is_reconciled_without_another_post() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let gitlab = FakeGitlab::live();
    let mut forgejo = FakeForgejo {
        lose_create: true,
        ..Default::default()
    };
    let mut transport = FakeTransport;
    let mut objects = FakeObjects::default();
    assert!(import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal
    )
    .is_err());
    assert!(journal.load(&config().key(7)).unwrap().create_attempted);
    forgejo.lose_create = false;
    let result = import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
    )
    .unwrap();
    assert_eq!(result["pull_number"], 20);
    assert_eq!(forgejo.posts.get(), 1);
}

#[test]
fn unknown_creation_without_visible_pr_blocks_retry() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let gitlab = FakeGitlab::live();
    let forgejo = FakeForgejo {
        reject_create: true,
        ..Default::default()
    };
    let mut transport = FakeTransport;
    let mut objects = FakeObjects::default();
    assert!(import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal
    )
    .is_err());
    let error = import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
    )
    .unwrap_err();
    assert!(error.to_string().contains("refusing a second POST"));
    assert_eq!(forgejo.posts.get(), 1);
    assert_eq!(objects.heads.len(), 1);
}

#[test]
fn closed_primary_is_not_reopened_or_pushed_and_source_is_not_closed() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let gitlab = FakeGitlab::live();
    let forgejo = FakeForgejo::default();
    let mut transport = FakeTransport;
    let mut objects = FakeObjects::default();
    import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
    )
    .unwrap();
    forgejo.pulls.borrow_mut()[0].state = "closed".into();
    let report = import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
    )
    .unwrap();
    assert_eq!(report["status"], "primary_closed");
    assert_eq!(report["source_close_pending"], true);
    assert_eq!(objects.heads.len(), 1);
    assert_eq!(forgejo.posts.get(), 1);
}

#[test]
fn unsafe_ci_preflight_stops_before_git_or_post() {
    for (actions, hooks) in [(true, false), (false, true)] {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::open(root.path()).unwrap();
        let gitlab = FakeGitlab::live();
        let forgejo = FakeForgejo {
            actions,
            hooks,
            ..Default::default()
        };
        let mut transport = FakeTransport;
        let mut objects = FakeObjects::default();
        assert!(import(
            &gitlab,
            &forgejo,
            &mut transport,
            &mut objects,
            &config(),
            7,
            &journal
        )
        .is_err());
        assert!(objects.heads.is_empty());
        assert_eq!(forgejo.posts.get(), 0);
    }
}

#[test]
fn private_source_cannot_be_imported_into_public_destination() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let gitlab = FakeGitlab {
        project_private: true,
        ..FakeGitlab::live()
    };
    let forgejo = FakeForgejo::default();
    let mut transport = FakeTransport;
    let mut objects = FakeObjects::default();
    let error = import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
    )
    .unwrap_err();
    assert!(error.to_string().contains("visibility"));
    assert!(objects.heads.is_empty());
    assert_eq!(forgejo.posts.get(), 0);
}

#[test]
fn changed_source_or_failed_git_never_creates_pr() {
    for moved_source in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::open(root.path()).unwrap();
        let mut gitlab = FakeGitlab::live();
        gitlab.moved_sha = moved_source;
        let forgejo = FakeForgejo::default();
        let mut transport = FakeTransport;
        let mut objects = FakeObjects {
            fail: !moved_source,
            ..Default::default()
        };
        assert!(import(
            &gitlab,
            &forgejo,
            &mut transport,
            &mut objects,
            &config(),
            7,
            &journal
        )
        .is_err());
        assert_eq!(forgejo.posts.get(), 0);
        assert!(!journal.load(&config().key(7)).unwrap().create_attempted);
    }
}

#[test]
fn forged_marker_wrong_actor_retarget_or_duplicate_pr_fails_closed() {
    for alteration in ["actor", "fork", "base", "marker", "duplicate"] {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::open(root.path()).unwrap();
        let gitlab = FakeGitlab::live();
        let forgejo = FakeForgejo::default();
        let mut transport = FakeTransport;
        let mut objects = FakeObjects::default();
        import(
            &gitlab,
            &forgejo,
            &mut transport,
            &mut objects,
            &config(),
            7,
            &journal,
        )
        .unwrap();
        match alteration {
            "actor" => forgejo.pulls.borrow_mut()[0].user_id = 99,
            "fork" => forgejo.pulls.borrow_mut()[0].head_repo_id = 99,
            "base" => forgejo.pulls.borrow_mut()[0].base_ref = "other".into(),
            "marker" => forgejo.pulls.borrow_mut()[0].body = Some("altered".into()),
            _ => {
                let duplicate = forgejo.pulls.borrow_mut()[0].clone();
                forgejo.pulls.borrow_mut().push(duplicate);
            }
        }
        assert!(
            import(
                &gitlab,
                &forgejo,
                &mut transport,
                &mut objects,
                &config(),
                7,
                &journal
            )
            .is_err(),
            "{alteration}"
        );
        assert_eq!(objects.heads.len(), 1);
        assert_eq!(forgejo.posts.get(), 1);
    }
}

#[test]
fn rejects_bad_source_identity_and_closed_mrs() {
    for (field, value) in [
        ("state", "closed"),
        ("iid", "8"),
        ("project_id", "1"),
        ("target_project_id", "1"),
        ("target_branch", "release"),
        ("sha", "-option"),
        ("title", "bad\ntitle"),
    ] {
        let mut mr = mr_fixture();
        match field {
            "state" => mr.state = value.into(),
            "iid" => mr.iid = value.parse().unwrap(),
            "project_id" => mr.project_id = value.parse().unwrap(),
            "target_project_id" => mr.target_project_id = value.parse().unwrap(),
            "target_branch" => mr.target_branch = value.into(),
            "sha" => mr.sha = value.into(),
            "title" => mr.title = value.into(),
            _ => unreachable!("fixture field"),
        }
        assert!(mr.validate(&config(), 7).is_err(), "{field}");
    }
}

#[test]
fn bodyless_and_authorless_notes_filter_out_instead_of_failing() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    let gitlab = FakeGitlab::live();
    // Malformed notes never match and never fail the listing: the run
    // continues past them exactly like the untyped reads did.
    gitlab.notes.replace(vec![
        GitlabNote {
            id: None,
            body: None,
            author_id: None,
        },
        GitlabNote {
            id: Some(7),
            body: Some("unrelated discussion".into()),
            author_id: None,
        },
    ]);
    let forgejo = FakeForgejo::default();
    let mut transport = FakeTransport;
    let mut objects = FakeObjects::default();
    import(
        &gitlab,
        &forgejo,
        &mut transport,
        &mut objects,
        &config(),
        7,
        &journal,
    )
    .unwrap();
    forgejo.pulls.borrow_mut()[0].state = "closed".into();
    forgejo.pulls.borrow_mut()[0].merged = Some(true);
    let result =
        lifecycle::feedback(&gitlab, &forgejo, &mut transport, &config(), 7, &journal).unwrap();
    assert_eq!(result["status"], "source_closed");
    // The malformed notes were skipped, so exactly one fresh note posted.
    assert_eq!(gitlab.notes.borrow().len(), 3);
    assert_eq!(gitlab.posted_bodies.borrow().len(), 1);
}

#[test]
fn validates_origins_and_repository_paths_before_any_io() {
    for value in [
        "http://forge.example",
        "https://user@forge.example",
        "https://forge.example/path",
        "https://forge.example?x",
        "https://forge.example\n",
        "https://forge.example:0",
    ] {
        assert!(origin(value).is_err(), "{value:?}");
    }
    for value in [
        "team/../repo",
        "team/repo?x",
        "team/%2F",
        "-o/repo",
        "team//repo",
    ] {
        assert!(repository(value, false).is_err());
    }
    config().validate().unwrap();
    let mut other = config();
    other.gitlab = "https://other.example".into();
    assert_ne!(config().key(7), other.key(7));
}

#[test]
fn journal_serializes_invocations_and_persists_poll_cooldown() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path()).unwrap();
    assert!(Journal::open(root.path()).is_err());
    journal.admit().unwrap();
    assert!(journal.admit().is_err());
    drop(journal);
    assert!(Journal::open(root.path()).unwrap().admit().is_err());
}

struct EndlessPages {
    calls: usize,
}

impl BridgeTransport for EndlessPages {
    fn execute(&mut self, request: &BridgeRequest) -> Result<Value> {
        self.calls += 1;
        assert!(request.path.ends_with(&format!("page={}", self.calls)));
        Ok(json!(vec![json!({"id":self.calls}); 50]))
    }
}

struct CappedPages {
    calls: usize,
    repeat_second: bool,
}

impl BridgeTransport for CappedPages {
    fn execute(&mut self, request: &BridgeRequest) -> Result<Value> {
        self.calls += 1;
        assert!(request.path.ends_with(&format!("page={}", self.calls)));
        Ok(match self.calls {
            1 => Value::Array(vec![json!({"id": 1}); 50]),
            2 if self.repeat_second => Value::Array(vec![json!({"id": 1}); 50]),
            2 => json!([{"id": 2}]),
            _ => json!([]),
        })
    }
}

fn page_request() -> (String, &'static str, BridgeAuth, String, &'static str) {
    (
        "https://forge.example".into(),
        "CFRG_BRIDGE_FORGEJO_TOKEN",
        BridgeAuth::Token,
        "/api/v1/repos/team/project/pulls?state=all".into(),
        "limit",
    )
}

#[test]
fn full_pagination_is_required() {
    let mut transport = EndlessPages { calls: 0 };
    // Twenty full distinct pages still refuse: only an empty page proves
    // completion, so the bound stops the reconciliation instead.
    let (origin, token, auth, path, param) = page_request();
    assert!(fetch_pages(&mut transport, &origin, token, auth, &path, param).is_err());
    assert_eq!(transport.calls, 20);
}

#[test]
fn server_capped_short_pages_are_followed_and_repeated_pages_are_rejected() {
    let (origin, token, auth, path, param) = page_request();
    let mut following = CappedPages {
        calls: 0,
        repeat_second: false,
    };
    let values = fetch_pages(&mut following, &origin, token, auth, &path, param).unwrap();
    assert_eq!(values.len(), 51);
    let mut repeating = CappedPages {
        calls: 0,
        repeat_second: true,
    };
    assert!(fetch_pages(&mut repeating, &origin, token, auth, &path, param).is_err());
}

fn git_command(root: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Contributor")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Contributor")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().into()
}

#[test]
fn git_preserves_objects_fast_forwards_and_refuses_rebases_or_sha_races() {
    let source = tempfile::tempdir().unwrap();
    let destination = tempfile::tempdir().unwrap();
    git_command(source.path(), &["init", "--initial-branch=main"]);
    git_command(destination.path(), &["init", "--bare"]);
    fs::write(source.path().join("payload.sh"), "exit 99\n").unwrap();
    git_command(source.path(), &["add", "payload.sh"]);
    git_command(source.path(), &["commit", "-m", "Original contribution"]);
    let first = git_command(source.path(), &["rev-parse", "HEAD"]);
    git_command(
        source.path(),
        &["update-ref", "refs/merge-requests/7/head", &first],
    );
    let make_transport = || {
        let mut transport = git::Git::new(Duration::from_secs(60)).unwrap();
        transport.allow_file_protocol = true;
        transport
    };
    let endpoint = || {
        (
            GitSource {
                url: source.path().to_string_lossy().into(),
                token_env: None,
                username: String::new(),
                mr_ref: "refs/merge-requests/7/head".into(),
            },
            GitDest {
                url: destination.path().to_string_lossy().into(),
                token_env: None,
                username: String::new(),
            },
        )
    };
    let branch = format!("ccid-import/gitlab/{}", config().key(7));
    let reference = format!("refs/heads/{branch}");
    let (from, to) = endpoint();
    make_transport()
        .publish(&from, &to, &"b".repeat(40), &branch)
        .unwrap_err();
    make_transport()
        .publish(&from, &to, &first, &branch)
        .unwrap();
    make_transport()
        .publish(&from, &to, &first, &branch)
        .unwrap();
    assert_eq!(
        git_command(destination.path(), &["cat-file", "-p", &first]),
        git_command(source.path(), &["cat-file", "-p", &first])
    );
    git_command(
        source.path(),
        &["commit", "--allow-empty", "-m", "Follow-up"],
    );
    let second = git_command(source.path(), &["rev-parse", "HEAD"]);
    git_command(
        source.path(),
        &["update-ref", "refs/merge-requests/7/head", &second],
    );
    make_transport()
        .publish(&from, &to, &second, &branch)
        .unwrap();
    assert_eq!(
        git_command(destination.path(), &["rev-parse", &reference]),
        second
    );
    git_command(
        source.path(),
        &["commit", "--amend", "--allow-empty", "-m", "Rebased"],
    );
    let rebased = git_command(source.path(), &["rev-parse", "HEAD"]);
    git_command(
        source.path(),
        &["update-ref", "refs/merge-requests/7/head", &rebased],
    );
    let error = make_transport()
        .publish(&from, &to, &rebased, &branch)
        .unwrap_err();
    assert!(error.to_string().contains("no force push"));
    assert_eq!(
        git_command(destination.path(), &["rev-parse", &reference]),
        second
    );
    assert!(!destination.path().join("payload.sh").exists());
    assert!(
        !git_command(destination.path(), &["for-each-ref", "--format=%(refname)"])
            .contains("refs/heads/main")
    );
}

#[test]
fn list_names_open_merge_requests_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    let config_path = root.path().join("bridge.toml");
    fs::write(
        &config_path,
        "schema = 1\n\
         gitlab = 'https://gitlab.example'\n\
         gitlab_project = 42\n\
         gitlab_repository = 'team/project'\n\
         forgejo = 'https://forge.example'\n\
         primary_repository = 'team/project'\n\
         import_repository = 'bridge/project'\n\
         target_branch = 'main'\n",
    )
    .unwrap();
    let options = Options {
        enable_gitlab_import: true,
        config: config_path,
        mr: None,
        apply: false,
        confirm_ci_disabled: false,
        state_dir: root.path().join("state"),
        feedback: false,
        replace_head: None,
        ci_plan: None,
        approval_message: false,
        dispatch_ci: false,
        kubernetes_command: None,
    };
    let mut gitlab = FakeGitlab::live();
    gitlab.refs = vec![7, 9];
    let forgejo = FakeForgejo::default();
    let report = run(options, &gitlab, &forgejo).unwrap();
    assert_eq!(report["status"], "listed");
    assert_eq!(report["merge_requests"].as_array().unwrap().len(), 2);
    assert_eq!(gitlab.calls.borrow().clone(), vec![GitlabCall::OpenList]);
    assert!(forgejo.calls.borrow().is_empty());
}
