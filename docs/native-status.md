# Native job status reporting

`cfrg status` is a separate one-shot reporter. The check executor neither calls
forge APIs nor receives `CFRG_STATUS_*` variables. Configure reporters only in
the trusted scheduler environment, never from contributed files. Generated Crow
adapters have pending and terminal reporter steps; unconfigured adapters retain
their existing behavior. Terminal steps run for successful and failed jobs.

The operator supplies `CFRG_STATUS_BINARY` and its verified
`CFRG_STATUS_BINARY_SHA256`, `CFRG_STATUS_CONFIG`, and `CFRG_STATUS_STATE_DIR`.
The latter is persistent and shared across invocations for provider pacing and
result ordering. Configuration contains no credentials:

```toml
schema = 1
[[targets]]
provider = "forgejo"
origin = "https://forge.example"
repository = "team/project"
token_env = "CFRG_STATUS_FORGEJO_TOKEN"
[[targets]]
provider = "gitlab"
origin = "https://gitlab.example"
repository = "group/subgroup/project"
token_env = "CFRG_STATUS_GITLAB_TOKEN"
[[targets]]
provider = "bitbucket"
origin = "https://api.bitbucket.org"
repository = "workspace/project"
token_env = "CFRG_STATUS_BITBUCKET_TOKEN"
```

```sh
cfrg status --config /operator/status.toml --state-dir /operator/status-state \
  --commit "$SHA" --name ccid/verify --url "$RUN_URL" \
  --started "$SCHEDULER_CREATED_UNIX" --state pending
```

After execution invoke the same command with `success` or `failure`. Keep the
same scheduler creation time and run URL. Each configured destination is checked
for the full commit identity first. A 404 is reported as `commit_absent`; retry
the same invocation after replication catches up. Other errors are visible and
do not prevent attempts at the remaining destinations. An identical successful
post is not repeated, older runs cannot overwrite newer ones, and a terminal
result cannot be replaced by pending or a conflicting terminal result.

There is a two-second persistent interval between requests to each provider.
HTTP 429 persists a cooldown (numeric Retry-After, or a conservative five-minute
fallback). Redirects, implicit retries, ambient curl configuration and proxies
are disabled. Credentials enter only the corresponding curl child's environment.
No GitHub provider exists; GitHub origins are rejected.

A durable intent precedes every POST. An uncertain or failed POST blocks another
POST of that transition; inspect the forge and reconcile the intent deliberately.
The reporter does not invent successful delivery, autonomously delete uncertain
state, or treat a reporting failure as a passing check. Scheduler cancellation
and hard worker loss still need controller reconciliation; an exit step alone
cannot guarantee execution after machine loss. State directories must be private
and trusted, and shared by every reporter using the same provider credentials.

API contracts: [GitLab commit statuses](https://docs.gitlab.com/api/commits/#set-commit-pipeline-status),
[Bitbucket Cloud build statuses](https://developer.atlassian.com/cloud/bitbucket/rest/api-group-commit-statuses/),
and the destination Forgejo instance's `/swagger.v1.json`.
