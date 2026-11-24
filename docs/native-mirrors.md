# Native push mirrors

`cfrg native --placement placement.json --state native-state.json` plans native
Forgejo replication. `--operation reconcile --apply` ensures declared GitLab or
Bitbucket destinations and reconciles owned Forgejo push mirrors. `--operation
status` lists redacted status; `--operation sync-now --apply` invokes Forgejo's
repository-wide sync endpoint only after every remote passes ownership and
protection checks. API create enables `sync_on_commit` (unless the destination declares `"sync_on_commit": false`: a destination paced by a gate then receives one push per interval, however many commits came in) and the declared interval.
Changes to mirror options replace the owned remote because Forgejo exposes no
update operation. Declare `absent: true` and its `remote_name` to delete a mirror;
this never deletes a destination repository. A declared hold means do not
touch: reconciliation never removes, replaces or creates a mirror of a held repository or
destination (it reports `held` or `held-untouched`).
Omission is not deletion.

A Bitbucket destination whose Forgejo mirror logs in under another name than the user the branch
restrictions admit (the access-token login `x-token-auth`) declares the admitted user as
`restriction_user` (a user uuid); without it the restrictions admit `mirror_user`. The mirror
password (`password_env`) is read from the credentials directory like the API token, the environment
only as the fallback of a hand run.

Keep the desired placement in the repository that owns deployment configuration.
Use `--provider gitlab --existing-only` to enroll existing projects while project
creation is cooling down. Missing destinations are reported without POSTing;
existing metadata and protection still reconcile. Provider filtering cannot
bypass sync-now's authorization check for every actual remote on the source.
Pass credentials by the named environment references. Keep mutable state on
persistent private storage, outside Git; it contains rate windows, pacing,
uncertain mutation intents, never credentials. Mirror ownership is not stored: a push mirror
is owned when its address is a declared destination of the repository (or carries a declared
`remote_name`); every other one is `foreign` in the report and is never changed.
`--operation adopt` is the read-only adoption listing: every push mirror as `owned` or
`foreign`, with counts per forge. It writes nothing and refuses `--apply`.

## Transfers: one action per move

A move between namespaces needs one action, either the Forgejo transfer (the pass finds it through the `source_id` and reports
`source-renamed`) or one change of the placement's `org_map`. Everything else is done by `plan` / `reconcile --apply`.

`org_map` maps the lowercase Forgejo organisation to the namespace it has on each provider (GitLab group, nested allowed;
Bitbucket workspace; GitHub owner). A destination that is not pinned lives at `<mapped namespace>/<repository>`; a provider
without an entry keeps the organisation's own name. It is also the opt-in: **only a repository whose organisation has an entry
for that provider is moved automatically.** Any other namespace change is reported `transfer-needed` and nothing moves implicitly.
(To move into an organisation that needs no renaming, give it an identity entry.) The Bitbucket project of a new repository
is the creation destination's `namespace` (the public/private project rule of `bitbucket_creation_dest` applies).

```json
"org_map": {"corbet-web": {"bitbucket": "corbet-archv", "gitlab": "corbet-archv", "github": "corbet-archv"}}
```

A GitHub entry also changes the owner that every GitHub destination of that organisation must name as `namespace`; otherwise
the validation of the whole placement fails (it is exact-case and global, so one colliding entry stops the run before anything
is written: fail closed).

Adapter capability `transfer`. A transfer changes the namespace only; a changed name as well is reported `transfer-blocked`
(transfer first, then rename). **Transfers write only under `reconcile --apply`.** `plan` never writes, and `rename --apply`
(the serve lane's `source-renamed` reaction) never transfers and never creates: it reports `transfer-needed`. States:
`transfer-planned` (with the steps), `transferred`, `transfer-pending` (a step was taken; the next pass continues),
`transfer-blocked` (reported, never forced), and `transfer-needed` (also in `--operation names`). Every transfer row also carries `provider` and `old_repository_id` (the destination's identity before the move), so a placement update matches the right destination. A provider's refusal reaches
the report as its HTTP status only: the transport never parses error bodies.

| Provider | Transfer |
|---|---|
| GitLab | `PUT /projects/:id/transfer` to the target group; the id stays and the old path redirects. The target group must exist; a taken path blocks. |
| GitHub | `POST /repos/:owner/:repo/transfer` to the target owner. Fails closed: a write needs `--github-kill-switch`, `--github-paused-dir` AND `--github-gate-catalog`; the kill switch, a paused source or target organisation and a flagged organisation each block. ONE transfer per run, whatever it ends in, and at least three minutes apart: the next due time is kept in the state file (`next_github_transfer`) and a transfer not yet due is `transfer-blocked: paced` (never slept on). The request is recorded in the state before it is sent and read back by id afterwards; a transfer requested earlier and not yet visible is `transfer-pending` and is never requested again. GitHub is a last resort: see the pacing policy before any real run. |
| Bitbucket | No public cross-workspace transfer exists. One step per pass: (1) create the repository in the target workspace and project, with the description `cfrg-transfer-from:<old uuid>`, record old uuid to new uuid in the state, switch its pipelines off, add its branch restrictions and a SECOND owned mirror to it (the old mirror keeps running, so both stay comparable); (2) `transfer-pending` until the mirror has pushed; (3) compare EVERY branch and tag of both (names and hashes, both directions); (4) only when they match exactly and the old repository has refs, delete the old repository (addressed by its uuid, identity re-read right before the delete, read back after it), drop the old mirror and continue the pass under the new uuid. A repository at the wanted name that is neither marked nor the recorded one is never adopted or deleted. |

After a Bitbucket move the destination has a new uuid. Until the placement's `repository_id` is updated, every pass finds the old
uuid answering 404, sees the recorded successor at the new path and reports `transferred` again without writing; it does not fail.
The new uuid is in the report (`new_repository_id`, `detail.repository_id`) and in the state (`bitbucket_transfers`).

### Rollout notes

Before the first apply on production, run `--operation names` read-only: every `transfer-needed` row must be an intended move.
A move is applied only for organisations listed in `org_map`. Tests are mocked (`cfrg/src/native/transfer/tests.rs` and the
`bitbucket_move` tests in `cfrg-cli/src/native.rs`): no network.

- **The flagged list is strict on a write.** A GitHub transfer under `reconcile --apply` is refused when the gate catalogue
  or `orgs.json` beside it cannot be read and parsed, and organisation names are compared without case.
- **A move in flight** (Bitbucket, recorded in the state, not yet complete) is reported `transfer-pending` by `status`,
  `rename` and `sync-now`, which would otherwise see two owned mirrors; `plan` and `reconcile` continue it. `--existing-only`
  and `rename` create neither the new repository nor its second mirror.
- **A GitHub 5xx on the transfer POST** leaves the request recorded in the state (`github_transfers`, by repository id) so it is
  never sent twice. After reading the repository by id on GitHub, a human decides: if it did not move, delete that key from the
  state file's `github_transfers` (the next due time `next_github_transfer` still applies) and run again; if it moved, the next
  pass reads it back and clears the record itself.
- **Infra follow-up for the Bitbucket uuid (after `transferred`).** The placement's `repository_id` of that destination must
  become the new uuid (`detail.repository_id` of the `transferred` row, also `bitbucket_transfers` in the state), and its
  recorded `path` the new `<workspace>/<repo>`. Until then every pass reports `transferred` again without writing. The infra
  placement render (`forge-bridge-placement-render.sh`) / the id recorder (`placement-record-ids.py`) must read the `transferred`
  rows of the daily `reconcile` report and write exactly those two fields; neither knows `transferred` today.
- **First real move, corbet.ch (Bitbucket `corbet-web/corbet.ch` to `corbet-archv/corbet.ch`).** The row has
  `repository_id: null`, so `naming` answers `Current` and no transfer is ever reached. Procedure: (1) read-only
  `GET https://api.bitbucket.org/2.0/repositories/corbet-web/corbet.ch` (the bitbucket token, `curl --fail`, no write) and take its
  `uuid` (with braces); (2) edit the row: the declared repository `path` to the live Forgejo name (`corbet-archv/corbet.ch`; a stale
  declared path makes `--repository` fail on Forgejo's 301), and the Bitbucket destination's `"repository_id": "<that uuid>"`;
  keep the destination's recorded `path` `corbet-web/corbet.ch` (it tells cfrg the old workspace); (3) add
  `"org_map": {"corbet-archv": {"bitbucket": "corbet-archv"}}` inside `native`; (4) run read-only first:
  `cfrg native --placement <native view> --state <state.json> --credentials-dir <dir with forgejo-token and bitbucket-token> --operation plan --repository corbet-archv/corbet.ch --provider bitbucket`
  and expect one `transfer-planned` row with four steps and only GET requests; (5) only then, after the review,
  `--operation reconcile --apply`. The new repository goes into the project the derive policy names for a private repository
  (`PRV` in the Corbet placement), not into the destination's `namespace`. The Cloudflare Pages project `corbet-ch` is not touched.

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
| `plan` | looks the destination up by its immutable id (`repository_id`) and reports `rename-planned`, `rename-blocked` (name taken), `transfer-planned` (other namespace, with the steps) or nothing |
| `reconcile --apply` | renames the destination through its adapter, replaces the owned Forgejo mirror that points at the old name, removes the old SSH deploy key; a namespace change is a transfer (below) |
| `rename --apply` | the same, and only for destinations whose name is out of date; never creates a destination |

Adapter capability `rename` (`cfrg switch --capabilities`): GitLab native (the
project path, and the display name when it carried the same text; the old path
redirects), Bitbucket native (the repository name, the slug follows; the old slug
answers 404, so the repository is addressed by UUID), Forgejo as a destination
and GitHub reported only (its repositories are renamed in GitHub). A rename is refused and reported, never forced, when the
wanted name is taken by another repository. A namespace change is not a rename: see Transfers. The recorded `remote_name` of a replaced mirror
changes: the report prints the new one, and the declared placement keeps it
(`remote_name` is an explicit ownership claim; the address derives the rest).

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
A copy never runs CI and never runs Dependabot:
the reconcile always switches off GitHub Actions, GitHub's Dependabot alerts and
security updates, GitLab pipelines and Bitbucket Pipelines, on every destination.
There is no `ci` field any more; a placement file that still carries `"ci": ...`
on a destination is refused when it is loaded (`deny_unknown_fields`), so no
declaration can keep a copy's CI on. Dependabot version updates have no switch
of their own: they follow `.github/dependabot.yml`, which a copy carries only
because its primary does.

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

## Settings of a copy: `--operation settings`

A copy is written only when one of our Forgejo push mirrors points at it (ownership is derived from Forgejo's mirror list,
`cfrg::native::owned_by`); any other copy is reported as not owned and left untouched.

`cfrg native --placement placement.json --state native-state.json --operation settings`
brings every copy in line with its Forgejo primary. It reads only
unless `--apply` is given. Copies are found from the primaries, not from the placement:
every non-contributed repository the source lists should have a copy on GitHub, GitLab
and Bitbucket, at the primary's path unless the placement pins another. A copy that
does not exist is reported `absent` and never created here. A contributed project (a
pull-mirrored repository, or a location the placement's `contributed` list names, on the
source or on the copy's forge) is skipped and reported `contributed-project`. A
hold on a repository holds its copies on every forge, declared or not.

| Setting | GitHub | GitLab | Bitbucket |
|---|---|---|---|
| visibility | `private` | `visibility` | `is_private` |
| description | yes | yes | yes |
| homepage | `homepage` | none (reported `unsupported`) | `website` |
| topics | yes (lowercase, digits, hyphens; at most 20, else held) | yes | none (`unsupported`) |
| default branch | only once the branch is on the copy | same | same |
| archived | yes, and only once the copy is at the primary's head | yes, same | none (`unsupported`) |
| issues on | `has_issues` | `issues_access_level` | none: Bitbucket Cloud removed issues on 2026-08-20 |
| wiki | **off** (edit rights cannot be restricted through the API; the description links to the Forgejo wiki) | on (the Forgejo wiki is pushed there) | none: removed on 2026-08-20 |
| CI off | Actions switch | `builds_access_level` | pipelines |
| update tooling off | Dependabot security updates | none | none |
| private vulnerability reporting | on, public copies only | none | none |

Visibility has a direction. A copy more open than its primary is narrowed at once, the safe
direction. A copy that would have to become public is **held and reported**
(`publishing-needs-approval`): this pass never publishes, because publishing cannot be undone.
CI is switched off on every copy that reports it on; nothing can declare it on. The picture and the
front-page presentation travel with the git content or need a multipart upload, and are not
written by this pass; licence is read from the repository's own file and follows the content.

Writes are paced like every other native mutation (default twelve seconds apart), related
fields travel in one request, an archived copy is unarchived first and archived last, and one
run writes to at most `--max-writes` copies (default 20). GitHub writes also honour the write
gate's files: `--github-kill-switch DIR` (no GitHub copy is written while it exists) and
`--github-paused-dir DIR` (`DIR/<org>` pauses that organisation); an `--apply` that reaches
GitHub refuses to start without both. The first 401, 402, 403,
429 or server error of a copy's forge stops that forge for the rest of the run (no retry,
the window is recorded in the state); a failed read of the Forgejo primary fails its row only. The report carries `totals` and `by_organisation` counts per
provider: copies, absent, converged, to change, held, unsupported, and the changes by field.

Dependabot's version updates cannot be switched off through an API: they start from
`.github/dependabot.yml` in the content, and a mirror carries the content unchanged. The
pass can switch security updates off, not version updates; a primary that carries the file
has to lose it (the updater runs on Forgejo).

## Members of a copy: `--operation members`

Reads the primary's non-owner team members and repository collaborators,
applies the one-way role rule of `cfrg::copy_access` and prints the grants it would make and
the members it cannot place. GitHub: triage. GitLab: Reporter (reads the code, manages
issues). Bitbucket has no triage role, so a private copy gets read and a public one nothing.
No role this module can produce may push or merge. `--identities FILE` is the identity table
(person id, one handle per forge); a member without an entry, or without an account on a
forge, is reported and never guessed. `--copies-from REPORT` limits the plan to the copies a
`settings` run found. Without `--apply` it is read-only. With `--apply` it writes the GitHub grants (GitHub handles only for now), each as one paced
collaborator write with the role triage, at most `--max-writes` (5) per run: a copy not on GitHub yet, or a handle that already
holds triage or more, is reported and not written, and no role is ever lowered. The write uses the owner's token from
`CFRG_GITHUB_MEMBERS_TOKEN`, not the GitHub App's (a collaborator write needs Administration: write, which the App does not hold).
The default-branch lock of every copy is the receiver lock above.

## Protection of a copy: `--operation protection`

Read-only, one request per copy. For the copies a `settings` run found (`--copies-from REPORT`) it reports whether the
default branch is held for the mirroring alone. Writing the lock is the job of `--operation reconcile`. Code:
`cfrg-cli/src/native/protection.rs`.

## Wiki of a copy: `--operation wiki`

Pushes the Forgejo wiki to the copies that can hold it: declared GitLab destinations and public GitHub destinations;
private GitHub and Bitbucket copies get a link in the description instead. The push is a fast-forward or nothing, never
forced; a diverged copy is held and reported. Without `--apply` it only reads and prints the plan. `--wiki-key` is the SSH
deploy key (private half, `<key>.pub` beside it), `--wiki-known-hosts` pins the host key, `--wiki-key-id` is the id of the
deploy key on GitLab (a copy without it enables the existing key by id), `--wiki-max-pushes` and `--wiki-push-gap-secs`
bound one run. `cfrg-serve` runs the same code for one repository when Forgejo's `wiki` event arrives. Code:
`cfrg/src/native/wiki_sync.rs`, `cfrg-cli/src/native/wiki.rs`, `cfrg-serve/src/wiki.rs`.

## Derived placement: `derive`

`"derive"` in the placement is the second POLICY over the forge's listing: every repository the source lists that the
placement does not declare and that is not a contributed project (R10, R11) gets a row computed from its own properties,
in memory, on every `cfrg native` pass (all operations, including `settings`, `wiki`, `protection` and `members`). The
declared `repositories` keep only real exceptions: a declared row (matched by path or by source id) replaces the
derivation completely, even with no destinations.

| Property | Derived |
|---|---|
| owner | `derive.owners[owner]`: the GitLab group id and the Bitbucket project key |
| visibility | the row's `private`; the settings pass (R42) makes every copy match it |
| content | `lfs` when `.gitattributes` of the default branch has `filter=lfs` (pointers-only copies, R4; with `bitbucket_lfs_hold` set the Bitbucket destination carries that hold), `profile` for `.github`, else `native-git` |
| no row | declared, contributed, excluded, fork, pull mirror, empty repository (its first commit places it, see `/hooks/place`), an owner that is neither mapped nor an organisation |
| never | GitHub (paced, gate-managed, stays declared); any CI on a copy (R6, R7) |

```json
"derive": {
  "gitlab":    {"endpoint": {"origin": "https://gitlab.com", "token_env": "CFRG_GITLAB_TOKEN"}, "mirror_user": "3382224",
                "password_env": "CFRG_GITLAB_TOKEN", "use_ssh": true, "interval_seconds": 3600},
  "bitbucket": {"endpoint": {"origin": "https://api.bitbucket.org", "token_env": "CFRG_BITBUCKET_TOKEN"}, "mirror_user": "{uuid}",
                "password_env": "CFRG_BITBUCKET_TOKEN", "interval_seconds": 3600},
  "bitbucket_lfs_hold": "reason",
  "bitbucket_lfs_ssh": true,
  "profile_providers": ["bitbucket"],
  "exclude": ["*/delete-me-*", "ds-*/*"],
  "owners": {"corbet-infra": {"gitlab": "144006931", "bitbucket": "CORE"}, "corbet-knowledge": {"gitlab": "144007206", "bitbucket": false}}
}
```

`bitbucket_lfs_ssh` makes the Bitbucket destination of an LFS repository an SSH push mirror (history only, pointers-only copy) and replaces the hold. `profile_providers` limits where a `.github` repository goes (absent: everywhere). `exclude` keeps throwaway repositories out
(`*` matches any run of characters, lowercase `owner/repo`). A provider key of an owner that is `false` is a decision (no copy there). A key that is absent, or an organisation
missing from `owners`, is a HOLD: no copy is made, no error is raised, and the report carries ONE `derive-hold` row per
owner and provider with every affected repository. It clears itself on the first pass after the group or project is
added to `owners`. A rename follows the primary (the derived destination has no recorded name, so a renamed repository
gets a new copy; the old one is never deleted).

`cfrg native ... --repository OWNER/REPO` lists that one repository only (the `/hooks/place` event path).

## The copy: every repository and wiki on one plain git host

`"copy"` in the placement is a POLICY over the forge's own repository listing, not a list of repositories:
`cfrg native --operation reconcile --provider copy --apply` (no `--provider` runs it after the providers) gives
every repository the source lists an SSH push mirror to one plain git host, so a repository nobody wrote down is
covered too. Forgejo does the transfer (sync on commit, plus the interval as a safety net); cfrg provisions.

```json
"copy": {"host": "git-host.example", "user": "mirror", "admin_key_env": "CFRG_COPY_KEY",
         "known_hosts_env": "CFRG_COPY_KNOWN_HOSTS", "interval_seconds": 600}
```

`host` is `host` or `host:port`, `user` a login, both `*_env` references name FILES (the private key of the
receiver's admin channel and a known_hosts file that pins the host key; ssh runs with `StrictHostKeyChecking=yes`,
`BatchMode` and `IdentitiesOnly`). Each pass, per repository: `ensure <path> <default-branch> <wiki 0|1>` creates the
bare repository (and the wiki twin) and sets HEAD; then the push mirror is created if missing (or replaced when its
address, interval, SSH key or sync-on-commit differ, which also follows a renamed repository); finally
`admit <path> <ssh-ed25519 key>` hands Forgejo's generated public key to the receiver, for every mirror on every
pass (idempotent, heals a lost keys file). Commands go over the admin channel one per line on stdin and are answered
by one line each (`ok`, `ok wiki-created`, or `err ...`); the receiver never deletes a repository.

| Fact (verified on Forgejo 17.0.1) | Consequence |
|---|---|
| A new push mirror pushes nothing until the next commit or the interval | seed the receiver, or wait one interval |
| The wiki is pushed only if `git ls-remote <address>.wiki.git` succeeds when the mirror is CREATED, as the Forgejo process, without the mirror's key | the forge process needs a read-only probe key (`GIT_SSH_COMMAND`), and the twin must exist before the mirror; `ok wiki-created` on a repository that already has a mirror replaces the mirror |
| A wiki-only edit does not trigger sync on commit | a wiki follows within one interval (or with the next push to the repository) |
| Branches and tags are pushed, `refs/pull/*` are not | the copy has no pull request head refs |
| Archived repositories refuse a new push mirror | reported `archived`; seed them once (they cannot change) |
| Destinations on a private address are refused unless `[migrations] ALLOWED_DOMAINS` names them | `external,<host>` on the forge |

A read-only pass (`plan`, `status`, or `reconcile` without `--apply`) writes nothing and needs no receiver.

## GitHub as a destination

`"provider": "github"` declares a GitHub repository as a destination of Forgejo's
native HTTPS push mirror (GitHub has no mirror feature of its own; it stays a
target, never a primary, and takes part in no switch). The endpoint is always
`https://api.github.com`; `namespace` is the owner (organisation) login and
equals the first component of the path; `repository_id` is the numeric
repository ID; `mirror_user` is the login of the account whose token is both the
API credential (`token_env`) and the mirror credential (`password_env`). Reconcile:

* Ensures the repository (a missing one is created in the organisation, private
  as the primary is, with issues on, projects and wiki off; creation is paced by the
  creation window) and always switches GitHub Actions off on it, and the
  Dependabot alerts and security updates (reads first, writes only on drift).
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

## Bitbucket layout: workspace, PUB and PRV

A Bitbucket copy lives in the workspace named like the forge organisation, with the primary's visibility (R42). Bitbucket
adds a middle layer, the project, and a private project refuses a public repository, so cfrg derives the project from the
primary's visibility and nothing else: public goes to the project `PUB`, private to `PRV` (`derive.bitbucket_public_namespace`,
`derive.bitbucket_private_namespace`; a declared destination's own `namespace` is only the fallback when they are absent).
cfrg creates the project on demand, private exactly when the repository is. Repository URLs never depend on the project.

When the primary's visibility changes, `settings` changes the copy's visibility and `native` moves the copy between the
projects in the order Bitbucket allows: going public, the copy is moved to `PUB` first (a public project may hold a private
repository), then made public; going private, the copy is made private first and moved to `PRV` on the next pass (a held move
is reported as `held-until-private` in the `bitbucket-project` event). Workspaces follow the three cases of the owner's rule:
all repositories public, the workspace is public; all private, private; mixed, public (Bitbucket keeps the private
repositories private inside a public workspace).

## LFS repositories

A repository whose default branch uses Git LFS (`content: lfs`) is mirrored by `native` per destination: a destination is mirrored like a
native-git one when it is an SSH push mirror (history only) and the placement's `external_content` declares `pointers-only` for its forge
(R4, R64: the payloads stay on the primary). Create, mirror, protection, visibility and the Bitbucket PUB/PRV placement then come from the
normal flow. A destination of an LFS repository that does not meet both conditions carries real LFS content (the content replication's)
and is left alone; a repository with no mirrorable destination, with a hold, or a profile repository (`.github`) stays an `exception`.

A GitLab copy declared pointers-only gets Git LFS switched OFF (project setting `lfs_enabled=false`), on creation and when an existing
copy has drifted: with LFS on, GitLab rejects a push whose pointers name objects the project does not hold ("LFS objects are missing").
