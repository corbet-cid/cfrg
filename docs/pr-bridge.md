# Contribution bridge (experimental)

`cfrg bridge` is a separate one-shot binary, disabled unless
`--enable-gitlab-import` is present. It lists open GitLab MRs, plans one import,
or explicitly publishes its unchanged Git commits to a bot-owned Forgejo fork
and opens a WIP PR against the primary. Existing commit authorship is retained;
the PR names the GitLab author and links to the original discussion. No code,
build scripts, hooks, filters or submodules are checked out or executed.

The first increment supports GitLab reads and Forgejo imports only. It never
writes to GitLab, GitHub or Bitbucket, merges a PR, or starts CI. An external
scheduler may call the binary; ccid's check executor remains separate.

## Configuration

Use private operator configuration outside the contributing repositories:

```toml
schema = 1
gitlab = "https://gitlab.example"
gitlab_project = 42
gitlab_repository = "team/project"
forgejo = "https://forge.example"
primary_repository = "team/project"
import_repository = "bridge/project"
target_branch = "main"
```

The primary must belong to an organization. The pre-created import repository
must be a direct fork with the same repository name, owned by the authenticated
bridge account. Source project ID and path are cross-checked. Private/internal
source visibility cannot be weakened by the destination or fork.

Supply `CFRG_BRIDGE_GITLAB_TOKEN` (read API and repository access) and, for import,
`CFRG_BRIDGE_FORGEJO_TOKEN` (scoped repository/PR writes and hook inventory reads).
Use a dedicated account. Never pass tokens in URLs or flags. Existing Git and
curl 8.3+ are required; the binary installs nothing. Use the actual HTTPS origins;
redirects, ambient Git configuration, credential helpers and proxy settings are
not followed. Each child receives only its own forge credential.

```sh
# List open MRs; omitting --apply performs no remote writes.
cfrg bridge --enable-gitlab-import --config bridge.toml --state-dir bridge-state
# Plan one MR, without requiring Forgejo credentials.
cfrg bridge --enable-gitlab-import --config bridge.toml --state-dir bridge-state --mr 7
# Explicit import, after the CI preconditions below have been established.
cfrg bridge --enable-gitlab-import --config bridge.toml --state-dir bridge-state \
  --mr 7 --apply --confirm-ci-disabled
```

## CI boundary

Import checks that Actions are disabled on both repositories and that repository,
primary-organization and bridge-user webhook inventories are empty. Missing hook
permissions fail closed. These checks cannot detect instance hooks, external
pollers or a concurrent administrator changing CI settings. `--confirm-ci-disabled`
attests that these paths have also been disabled for the enrollment; repeatable
deployment policy must enforce this boundary. A WIP title alone is not a CI gate.

This initial import-only mode is unsuitable for a target with an active Crow
webhook. Future CI integration must require maintainer approval bound to the
exact imported head and current base, reset approval on updates, and execute in
an isolated worker without forge credentials, production secrets, shared writable
caches or deployment permissions. Import credentials never go into that worker.

## Reconciliation and limits

The mapping and MR IID hash into a stable `ccid-import/gitlab/<digest>` branch and
PR marker. The bridge fetches `refs/merge-requests/<iid>/head` from the configured
GitLab target project, including for fork MRs, and verifies the exact observed
SHA. It writes only that branch in the import fork. Default branches and tags
are untouched. Repeated imports reuse the PR; normal source updates fast-forward
the branch. Rebases/divergence stop without force-pushing. Titles and discussion
are not synchronized after initial creation; the body identifies the initial SHA.

Use one persistent private state directory and one bridge identity across all
invocations. An OS lock serializes callers. The journal atomically records and
fsyncs intent before PR creation. All PR states are searched, checking marker,
author, fork, branch and base. Lost responses can be recovered by finding the
created PR. An unresolved POST with no matching PR blocks another POST, including
after HTTP errors. Do not delete the intent to retry without an operator audit.
Deleted or edited bridge PR identities fail closed. Closed primary PRs are
reported and never reopened; closing the source with a backlink remains pending.

API calls are paced at least one second apart, bounded to 200 per invocation,
30 seconds per call and ten minutes overall.

### Allowed mutations

The bridge has no generic mutation path: every write is one typed
capability method, implemented once per adapter (`cglb`, `cfgj`), and the
orchestrator cannot express anything else. The complete set is:

| Operation | Method and endpoint |
|---|---|
| Source feedback note | `POST /api/v4/projects/{id}/merge_requests/{iid}/notes` with `{"body"}` |
| Source close | `PUT /api/v4/projects/{id}/merge_requests/{iid}` with `{"state_event":"close"}` (never the merge endpoint) |
| Import pull create | `POST /api/v1/repos/{primary}/pulls` with head/base/title/body |
| Predecessor close | `PATCH /api/v1/repos/{primary}/pulls/{index}` with `{"state":"closed","body"}` |

Git object writes go only through the import ref (`refs/heads/ccid-import/…`)
by fast-forward, verified after every push. All other API use is reads. Git operations have a five-minute
deadline. Pagination is bounded to 20 pages of 50, failing if incomplete. A
persistent 60-second polling floor survives restarts. The scheduler should add
longer backoff/jitter after failures and honor provider throttling; this binary
does not retry HTTP writes or follow redirects. Run one mapping at a time with
a shared provider budget. State directories on separate hosts do not coordinate.

The Git transport validates objects and avoids checkout, but is not a hostile
pack resource sandbox. Deploy the service with disk, memory and CPU limits and
current Git security updates. Historical Git objects retain their original
metadata; the bridge generates no commits or author email addresses. LFS and
submodule build closure, isolated CI dispatch, webhook ingestion, rebase
replacement policy and source-close feedback require later increments.

## Maintainer approval, isolated execution and feedback

`--ci-plan /operator/request.json` validates the live primary review and current
maintainer permissions, then emits a Kubernetes Job. This does not dispatch it.
`--approval-message` prints the exact review body needed for that request without
contacting a forge. The JSON request contains `head`, `base`, `review` (numeric
review ID), `maintainer`, `maintainer_id`, and `sandbox` with `namespace`, `image`,
`archive_sha256`, and the trusted `command` array. Namespace names start with
`ccid-untrusted-`; images require SHA-256 digests. The exact serialized sandbox
configuration is bound into the approval message alongside the head and base.
The source MR must still have that head. Dismissed/stale reviews and users who
have lost write permission are rejected. New heads, bases, commands, archives
or images require new primary approval.

To dispatch, add `--dispatch-ci --kubernetes-command /operator/kubernetes`.
The executable wrapper supplies the deployment's Kubernetes CLI. Its child
receives only the minimal transport environment and optional KUBECONFIG, never
forge tokens. Before creation the bridge verifies the live dedicated namespace:
restricted pod admission, exactly one deny-all NetworkPolicy, no secrets, PVCs
or role bindings, credential-free service account, bounded quota, and an immutable
source ConfigMap. It then rechecks primary approval and source identity. Creation
uses a deterministic Job name and a durable intent; unknown outcomes attach an
existing matching Job or stop, never submit another job blindly.

Stage a verified, complete source archive in immutable ConfigMap
`ccid-source-<first 32 characters of archive SHA256>`, as binaryData `source.tar`.
The archive digest is checked before extraction. LFS and submodules must already
be materialized and verified by trusted source staging; this runner performs no
network fetching. The image must already contain the selected check tools.
The Job receives no credentials, service-account token, host mounts, shared
writable caches or deployment access. It has a read-only root filesystem,
read-only source input, disposable bounded scratch, no capabilities, non-root
identity, a 15-minute deadline and no automatic retry. The initial sandbox
uses no persistent cache at all. The companion nixci `untrusted-ci` module owns
the namespace policies. Deployment must prove NetworkPolicy enforcement and
pod restrictions with runtime probes before enrollment. Rendering alone is not
isolation evidence. Kubernetes admission/webhook administration remains trusted.

`--feedback --apply --mr IID --confirm-ci-disabled` reconciles a journaled
primary merge or close, including after the source is already closed. It posts
a backlink comment with a durable intent, recovers lost comment responses from
a complete note inventory, and closes the secondary MR. It never calls the
secondary merge endpoint. A changed source head blocks closure; closing the
source is an idempotent operation verified by a fresh read. Cross-forge reads
and GitLab closure are not one atomic transaction; a change during the final
close is detected and requires review.

For a rebased contribution use `--replace-head FULL_SHA --apply --mr IID
--confirm-ci-disabled`. The bridge creates a new branch/PR generation, preserves
the original branch and commits, and closes the previous PR with a replacement
link. A repeat reconciles the same generation. The active journal points to the
replacement, so feedback cannot accidentally close the source based on the
superseded PR. No approvals are transferred. Subsequent changed heads of a
replacement require another explicit replacement. Normal initial-generation
updates still use ordinary fast-forward pushes.

Activation requires a dedicated bridge account, narrowly scoped tokens, audited
CI-disabled import repositories, a tested isolated namespace, immutable source
staging and a real primary maintainer approval. Do not deploy the broad operator
credential as the bridge identity. No timer or automatic discovery is installed
by these one-shot commands; operators schedule bounded invocations explicitly.
