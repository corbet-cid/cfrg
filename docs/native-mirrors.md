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

## Renames: destinations follow the primary

The name of a destination is the primary's current `<org>/<repo>`, unless the
placement pins a different path and says why (`"path"` together with
`"path_reason"`; a path without a reason is only a record of the last known name
and the destination follows the primary anyway, so a `path` may simply be left
out). Forgejo keeps a repository's numeric `source_id` through a rename or
transfer, so every pass looks the primary up by that id
(`GET /repositories/{id}`), not by the name written in the file. A name that
differs is reported as `source-renamed` (the declared placement is stale, the
destinations follow the live name regardless).

Forgejo has no webhook for a rename or a transfer (the `repository` event only
says `created` or `deleted`, verified against the 15.0 notifier), so renames are
found by the passes themselves: `cfrg serve` verifies every repository on a
push and at least every sweep, and `--operation status` reports a mirror that
still points at an earlier name as `rename-pending`.

| Operation | What a rename of the primary does |
|---|---|
| `status` | reads Forgejo only; `rename-pending` when a mirror points at an earlier name of the destination |
| `plan` | looks the destination up by its immutable id (`repository_id`) and reports `rename-planned`, `rename-blocked` (name taken, other namespace) or nothing |
| `reconcile --apply` | renames the destination through its adapter, replaces the owned Forgejo mirror that points at the old name, moves the ownership record, removes the old SSH deploy key |
| `rename --apply` | the same, and only for destinations whose name is out of date; never creates a destination |

Adapter capability `rename` (`cfrg switch --capabilities`): GitLab native (the
project path, and the display name when it carried the same text; the old path
redirects), Bitbucket native (the repository name, the slug follows; the old slug
answers 404, so the repository is addressed by UUID), Forgejo as a destination
and GitHub reported only (its repositories are renamed in GitHub). A rename is refused and reported, never forced, when the
wanted name is taken by another repository or when the namespace differs (a
transfer is not implemented). The recorded `remote_name` of a replaced mirror
changes: the report prints the new one, and the declared placement keeps it
(`remote_name` is the durable ownership claim; the state file is the cache).

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

## The receiver lock protects the default branch only

A destination is a receiver: only the mirror principal may write the default
branch, and no other branch is protected. The reason is deletion. Forgejo's push
mirror deletes on the destination every branch the primary no longer has, and
a forge refuses to delete a protected branch by push. GitLab declines the WHOLE
push then (`You can only delete protected branches using the web interface`,
verified live), so one protected branch that the primary removed freezes the
mirror: nothing else reaches the destination either, and every sync fails the
same way. A lock over `*` therefore turns every branch deletion on the primary
into a frozen mirror. The lock is one rule, on the default branch, and a branch
protected for any other pattern is removed by the reconcile (the destination is
a receiver, cfrg owns its protection). When the default branch moves, the next
reconcile locks the new one and removes the old rule.

| Forge | Receiver lock | Branch deletion by the mirror |
|---|---|---|
| GitLab Free | one protected branch rule named after the default branch: only the Forgejo deploy key (`use_ssh`) or, on Premium, the mirror user may push; merges denied; force-push on for that principal. Other rules are deleted after the default rule is verified | refused on a protected branch (the freeze above), allowed on the others |
| Bitbucket Free | branch restriction `push` on `*` for the mirror account plus `restrict_merges` for nobody | allowed: a push restriction names who may push, and that includes deleting (verified live on a probe repository: the branch deleted on Forgejo disappeared from Bitbucket while both restrictions stayed). Only a separate `delete` restriction forbids it, and such a restriction is refused |
| GitHub | one ruleset `cfrg receiver lock` on `~DEFAULT_BRANCH` with the rules `update`, `deletion` and `non_fast_forward`, bypass for organisation owners (the mirror principal must be one) | allowed on every other branch; no other ruleset or classic protection may exist |

GitLab supports SSH deploy-key branch protection on Free from 18.10. With
`use_ssh`, cfrg denies branch writes before creating the Forgejo-generated key,
adds only its public part to GitLab using the declared mirror account, and then
grants that key alone. No private key leaves Forgejo. HTTPS mirrors instead need
GitLab's named-user protection capability (Premium/Ultimate). Neither path
falls back to permitting a broad role. Existing rules of the default branch are
patched in place and re-read, with merges denied and force-push enabled only for
the mirror; the rules of other names go last, once the default branch is
verified exclusive.
Bitbucket restricts branch pushes to the declared account, denies merges and
refuses existing force/delete restrictions that would prevent native mirroring.
Administrative changes and destination tags are outside these branch checks.

## GitHub as a destination

`"provider": "github"` declares a GitHub repository as a destination of Forgejo's
native HTTPS push mirror (GitHub has no mirror feature of its own; it stays a
target, never a primary, and takes part in no switch). The endpoint is always
`https://api.github.com`; `namespace` is the owner (organisation) login and
equals the first component of the path; `repository_id` is the numeric
repository ID; `mirror_user` is the login of the account whose token is both the
API credential (`token_env`) and the mirror credential (`password_env`). Reconcile:

* Ensures the repository (a missing one is created in the organisation, private
  as the primary is, with issues, projects and wiki off; creation is paced by the
  creation window) and switches GitHub Actions off on it.
* Locks it as above. GitHub Free has rulesets for public repositories only: a
  private destination in a free organisation cannot be locked and is reported as
  `destination-or-protection-required`, never mirrored unprotected, unless the
  destination declares `"lock_exception": "<reason>"` (private GitHub repositories
  only): the mirror is then configured without the receiver lock and every pass
  reports `state: lock-exception, lock: unavailable`. The API
  credential must be the declared mirror principal and an organisation owner.
* Owns the Forgejo mirror like any other (`remote_name` in the placement, or the
  state's record). A rename of the GitHub repository is reported
  (`rename-blocked`), never applied.
* Verifies the heads: the `configured` row carries `heads` with the default
  branch's commit on the primary and on GitHub; a difference makes the pass
  incomplete.

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
[GitHub repository rules](https://docs.github.com/en/rest/repos/rules),
[GitLab deploy keys](https://docs.gitlab.com/api/deploy_keys/),
[Bitbucket branch restrictions](https://developer.atlassian.com/cloud/bitbucket/rest/api-group-branch-restrictions/).
