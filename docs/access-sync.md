# Access sync across forges

`cfrg access plan|apply` keeps repository access (organization
membership, teams, repository collaborators) identical on every forge for
every person who holds an account there. A grant or revocation may be made
on any forge; the sync mirrors it everywhere else. People are matched
through a declarative identity map; nothing is ever guessed by name or
email.

This command is offline by design in this draft: collectors supply one JSON
observation snapshot per forge, the planner prints the exact intended API
calls, and `apply` advances the local baseline state. The live transport is
unwired (`UnwiredTransport` refuses every write without network I/O), so no
provider API is touched. The apply engine already implements the full
safety contract — pacing, stop on the first 401/402/403/429, per-forge state advancement
— against the `Transport` trait; wire a reviewed live transport behind it
before performing real writes.

## Identity map

One TOML file, one entry per person: a stable person id with that person's
handle on each forge where they hold an account. Handles only, never emails
or secrets. See [`examples/identities.toml`](examples/identities.toml).

```toml
schema = 1
[people.alice-example]
github = "alice-gh"
forgejo = "alice-fj"
gitlab = "alice-gl"
```

Duplicate handles on one forge fail closed, as do unknown forges, empty
maps and more than 1024 people. Handles match case-insensitively; the
declared spelling is kept for API calls. Observed accounts with no map
entry are reported as unmapped and never touched.

Personal namespaces compare equal across forges through the map: an org
whose name is a mapped handle canonicalizes to `person:<id>`, so a personal
org held under different handles on different forges is still one target.
An org name claimed by two people's handles stays literal and is reported.
Executable calls always address the forge-local namespace (the person's
handle on that forge); cross-forge reports show the canonical form.

## Model

Each forge contributes grants of `{handle, org, team|repo, role}` with
exactly one of `team` or `repo`. Roles normalize to `read`, `write` or
`admin` before the merge; anything else fails the run closed:

| Forge     | read                                  | write                   | admin                              |
|-----------|---------------------------------------|-------------------------|------------------------------------|
| GitHub    | pull, triage, member, read            | push, maintain, write   | admin, owner                       |
| Forgejo   | read                                  | write                   | admin, owner                       |
| GitLab    | guest, reporter, minimal, 10, 20, read| developer, 30, write    | maintainer, owner, 40, 50, admin   |
| Bitbucket | viewer, read                          | member, developer, write| admin, owner                       |

GitHub team membership alone carries no permission, so collectors must
record the effective permission (`pull`/`push`/`admin` family): bare
`member` here means the default org-member read grant. Forgejo organization
membership is a team grant: a `Members` team with read over all
repositories mirrors GitHub organization membership with default read.
Repository grants are collaborators.

The table above is normative; the implementations live once per adapter
(`cghb`, `cfgj`, `cglb`, `cbkt`) behind the core `RoleMap` trait, with the
grant-write API shapes behind `Grants`. The planner itself never names a
forge.

## Three-way merge

The JSON state file records the last-synced baseline per forge. Changes on
each forge since the baseline become events (grant, level change, revoke).
Each event is applied to every other forge where the person has a mapped
account. Revocations propagate: a union of current states is wrong.

Grants are always exact mirrors. When the same person+target changed
differently on two or more forges since the baseline, the LATEST change
wins and is mirrored everywhere; the overwritten outcomes are reported. The
event time comes from the grant's `changed_at` audit timestamp when the
collector supplies one, else the snapshot's `observed_at`, else the run
time. An exact timestamp tie picks no winner: the conflict is reported and
a human must re-issue one side. Divergence with no events since the
baseline is reported as drift without writes — except that a present grant
versus absence elsewhere is tolerated as a partial rollout; only two
present levels that disagree count as drift.

A missing baseline plans nothing. Bootstrap it explicitly:

```sh
cfrg access apply --identities ids.toml --baseline state.json \
  --observed forgejo=fj.json --observed gitlab=gl.json --initialize
```

`--initialize` refuses to overwrite an existing baseline.

## Protections

- GitHub is frozen by default: read but never written (repeat `--frozen`
  for more forges). Planned GitHub writes are reported as `skipped_frozen`.
- Organization owners and namespace admins are never removed or downgraded
  (`owner-admin-protected`). The check covers personal-namespace admins: an
  admin grant under the person's own `person:<id>` namespace keeps admin.
- The operator's own access is never touched (`--operator <person-id>`,
  reported as `operator`).
- Forges with a mapped account but no snapshot are never assumed: needed
  mirrors there are reported as `unmapped_targets` with reason
  `forge-unobserved`.

## Running it

```sh
# Inspect only; exits nonzero while the plan is not converged.
cfrg access plan --identities ids.toml --baseline state.json \
  --observed forgejo=fj.json --observed gitlab=gl.json

# Advance the baseline once converged; otherwise fail closed.
cfrg access apply --identities ids.toml --baseline state.json \
  --observed forgejo=fj.json --observed gitlab=gl.json \
  --state-out state.next.json --pace-ms 500
```

`plan` prints the `AccessPlan` JSON: writable `actions` with the exact
method, path and body per call, plus `skipped_frozen`,
`skipped_protected`, `conflicts` (with the overwritten outcomes),
`drift`, `unmapped_accounts`, `unmapped_targets` and observation `notes`.
`complete` means no action remains and nothing is refused or drifting.

Every rendered call lists the id `lookups` (team slug/id, user id, project
or group id, account id) the transport must resolve first. Grants render as
`set-level` with a create method (notably GitLab `POST` on the collection);
changes render as `set-level` with an update method (`PUT`); revocations
render as `DELETE`. Every call carries `endpoint_verified: false` in this
draft: templates still need live verification against the official
references below before any transport ships.

Writes run paced with sleeps between calls. The first 401/403/429 stops the
whole run; any other error skips the rest of that forge and continues with
the next one. The state file advances per forge only after all of that
forge's writes succeeded, and origin forges advance only once their events
fully propagated — so the next run retries exactly the remainder.
Re-running a converged plan is a no-op (idempotent).

## Snapshot formats

Observation snapshot (one file per forge, supplied by a collector; times
are Unix epoch seconds):

```json
{"forge": "forgejo", "observed_at": 1759820000, "grants": [
  {"handle": "alice-fj", "org": "acme", "team": "dev",
   "role": "write", "changed_at": 1759810000}
]}
```

Exactly one of `team` or `repo` per grant. `changed_at` is optional.

The baseline state file stores the same grants canonicalized (person ids,
`person:<id>` namespaces, lowercase names) grouped per forge, each with its
event time `at`:

```json
{"schema": 1, "forges": {"forgejo": [
  {"person": "alice", "org": "acme", "team": "dev",
   "level": "write", "at": 1759810000}
]}}
```

## Endpoint references (unverified)

- GitHub team membership and repository collaborators:
  https://docs.github.com/en/rest/teams/members and
  https://docs.github.com/en/rest/collaborators/collaborators
- Forgejo API usage and Swagger reference:
  https://forgejo.org/docs/latest/user/api-usage/
- GitLab group and project members:
  https://docs.gitlab.com/api/members/
- Bitbucket Cloud REST (workspace members, repository permissions):
  https://developer.atlassian.com/cloud/bitbucket/rest/
