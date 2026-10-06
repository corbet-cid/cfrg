# Native push mirrors

`cfrg native --placement placement.json --state native-state.json` plans native
Forgejo replication. `--operation reconcile --apply` ensures declared GitLab or
Bitbucket destinations and reconciles owned Forgejo push mirrors. `--operation
status` lists redacted status; `--operation sync-now --apply` invokes Forgejo's
repository-wide sync endpoint only after every remote passes ownership and
protection checks. API create enables `sync_on_commit` and the declared interval.
Changes to mirror options replace the owned remote because Forgejo exposes no
update operation. Declare `absent: true` and its `remote_name` to delete a mirror;
this never deletes a destination repository. A declared hold removes owned push
mirrors during reconciliation so scheduled retries cannot bypass the hold.
Omission is not deletion.

Keep the desired placement in the repository that owns deployment configuration.
Use `--provider gitlab --existing-only` to enroll existing projects while project
creation is cooling down. Missing destinations are reported without POSTing;
existing metadata and protection still reconcile. Provider filtering cannot
bypass sync-now's authorization check for every actual remote on the source.
Pass credentials by the named environment references. Keep mutable state on
persistent private storage, outside Git; it contains rate windows, pacing,
uncertain mutation intents and remote ownership IDs, never credentials.

```json
{
  "schema": 1,
  "source": {"origin": "https://forge.example", "token_env": "FORGE_TOKEN"},
  "repositories": [{
    "path": "team/project", "source_id": 123, "private": true,
    "default_branch": "main", "content": "native-git", "hold": null,
    "destinations": [{
      "provider": "gitlab",
      "endpoint": {"origin": "https://gitlab.example", "token_env": "GITLAB_TOKEN"},
      "path": "team/project", "namespace": "42", "repository_id": "456",
      "mirror_user": "789", "password_env": "GITLAB_TOKEN", "use_ssh": true,
      "interval_seconds": 3600, "branch_filter": "", "hold": null,
      "absent": false, "remote_name": null
    }]
  }]
}
```

GitLab `namespace` is the existing numeric namespace ID. Its mirror principal
is a numeric user ID; Bitbucket uses the existing project key and account UUID.
Bitbucket uses `https://api.bitbucket.org`, HTTPS Git authentication, and disables
pipelines before enrollment. No accounts, namespaces or workspaces are created.
Leading-dot GitLab profiles, declared LFS/profile content, and explicit holds
remain exceptions. Content classification must come from reviewed source
inventory; this controller does not inspect Git objects. There is no LFS fallback.

GitLab supports SSH deploy-key branch protection on Free from 18.10. With
`use_ssh`, cfrg denies branch writes before creating the Forgejo-generated key,
adds only its public part to GitLab using the declared mirror account, and then
grants that key alone. No private key leaves Forgejo. HTTPS mirrors instead need
GitLab's named-user protection capability (Premium/Ultimate). Neither path
falls back to permitting a broad role. Existing overlapping rules are reconciled
and re-read, with merges denied and force-push enabled only for the mirror.
Bitbucket restricts branch pushes to the declared account, denies merges and
refuses existing force/delete restrictions that would prevent native mirroring.
Administrative changes and destination tags are outside these branch checks.

API calls are serialized with at least two seconds between requests, twelve
seconds between mutations, and sixty seconds between repository creations.
Reads cannot shorten either durable write deadline.
The first 429/402 stops execution and records its window. GitLab project creation
has a separate scope: existing-project reads/protection still work during that
cooldown. A Bitbucket 402 holds all writes in that workspace; reads remain
possible. Missing/non-numeric Retry-After stays held for review. Seed documented
existing windows in the state before first use. HTTP mutations are never retried
automatically. Transport failure or server error leaves a durable pending intent;
read provider state and resolve that intent deliberately before retrying.

Source numeric ID, full path, primary status, visibility and default branch are
verified before enrollment. Destination ID (when declared), path and visibility
must match. Existing undeclared mirrors require an explicit reviewed remote-name
claim. Logs omit remote URLs, remote errors and response bodies. A configured
mirror is not proof of successful replication; monitor `last_update`, `has_error`
and compare source/destination refs separately. The old Git transfer path must
remain deployed until native replication is proven, then be removed in the
deployment migration to avoid two writers.

API contracts: [Forgejo source](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/routers/api/v1/repo/mirror.go),
[GitLab protected branches](https://docs.gitlab.com/api/protected_branches/),
[GitLab deploy keys](https://docs.gitlab.com/api/deploy_keys/),
[Bitbucket branch restrictions](https://developer.atlassian.com/cloud/bitbucket/rest/api-group-branch-restrictions/).
