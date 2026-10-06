# Fast-forward-only reconciliation

`cfrg sync` reads an explicit repository policy and copies selected Git
refs from that policy's primary forge to explicitly selected secondary forges.
It uses existing Git credentials and transports. It does not provision repositories,
change the primary, install tools, or run a background service.

```sh
# Inspect every source branch and tag; no remote writes.
cfrg sync --policy forges.json --repository widget \
  --all-refs --destination backup

# Reinspect, then apply safe updates.
cfrg sync --policy forges.json --repository widget \
  --all-refs --destination backup --apply

# A deliberately bounded subset. Full names are required; wildcards are refused.
cfrg sync --policy forges.json --repository widget \
  --ref refs/heads/main --ref refs/tags/v1.0 --destination backup --apply
```

Choose `--all-refs` or one or more `--ref` values. Repeat `--destination` for each
declared secondary. The command refuses missing, duplicate, primary or undeclared
destinations. `--timeout` bounds the complete operation, including inventories,
object transfer, ancestry checks and verification.

Each invocation observes source object IDs, fetches their complete history into a
temporary bare repository, and examines every destination before writing. An absent
branch or tag may be created. An existing branch advances only when its current
commit is an ancestor of the observed source. An ahead or divergent branch blocks
that destination. Tags must match the exact tag object, including annotation;
an existing tag is never replaced. Under `--all-refs`, destination-only refs are
reported and retained, and block automatic promotion at that destination.

Updates use one atomic, ordinary Git push per eligible destination. There is no
force, mirror, prune or delete operation. Server-side races and atomic-push refusal
leave the destination incomplete. A lost push response is followed only by a read;
the command never blindly resubmits it. Exact post-push ref observation can prove
success even when the response was lost. Source movement during reconciliation
also leaves the request incomplete so a later invocation can catch up.

JSON reports distinguish `planned`, `current`, `updated`, `blocked` and `pending`.
An unreachable location remains pending; reachable, safe secondaries may still
advance. Running the command again after an outage reconciles from fresh evidence.
`complete` applies to the explicitly requested scope. `repository_complete` also
requires `--all-refs`, all declared secondary locations, no destination-only refs
and verified absence of external content requirements. Applied incomplete results
and blocked or unavailable locations return a failing exit status after the report.

This first implementation does **not** transfer LFS payloads or submodule
repositories. It inspects fetched source history, including files removed from the
current tree, for LFS pointer blobs and Git tree entries naming submodule commits.
Either requirement produces `external-content-unverified` and blocks all ref
promotion before writes. This prevents a successful Git push from being presented
as complete source replication. Large or unreadable object inspections fail closed.
An external-content transport and its verification receipts are required before
such repositories can use this command to promote replicas.

Git authentication failures, server protections and ambiguous transport failures
are reported as incomplete rather than guessed to mean a missing repository.
Provider-specific pacing, account holds and scheduling belong in the caller; the
command makes no forge API requests and never creates or changes credentials.

Transport uses the `.git` clone endpoint and refuses HTTP redirects. A secondary
cannot redirect a push to another location. User/system Git configuration and
trace settings are ignored; supply intentional transport rewrites through
`GIT_CONFIG_COUNT` and authentication through askpass or explicit SSH settings.
These caller-supplied settings, executable search paths and policy files are
trusted inputs. Remote stderr is discarded; reports expose fixed failure reasons
and process timing, without copying server diagnostics into logs.

## Profile repositories

Repositories whose final path segment starts with a dot (`.github`,
`.profile`, …) are never mirrored 1:1: their per-forge conventions do not
survive a byte copy. `cfrg sync` refuses them with a projection error.
Project their content instead with the pure `cfrg::profile` planners
(`project_profile` for org-wide defaults, `project_readme` for profile
READMEs, `project_description` for workspace-description forges), which map
canonical files across each forge's profile conventions only where the
target has none. Every output names its destination repository plus a
repo-relative path (`profile/README.md`, never `.github/profile/README.md`),
so an applier joins each exactly once. The planners produce data; no live
projection transport exists yet, so applying a projection remains an
explicit operator step.

Inventories are limited to 4096 refs and 1100 bytes per inventory line. Command
output is capped at 16 MiB and content inspection batches at 12 MiB. Oversized
or unreadable inputs fail closed. The caller must also bound memory, process
count and scratch storage; a deadline alone does not bound packfile size. The
NixOS reconciliation module supplies those resource controls and disables cores.
