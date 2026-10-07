# Replication with any primary, and the primary switch

The placement says which forge is a repository's primary. `cfrg switch` makes
the forges agree with it. Everything forge-specific lives in the adapters
(`cfgj`, `cglb`, `cbkt`, `cghb`); the procedure in `cfrg::switch` and the
vocabulary in `cfrg::replicate` name no forge.

## One edge, one mechanism

For every edge (sender to receiver) cfrg picks the most native mechanism:

| Order | Mechanism | Used when |
|---|---|---|
| 1 | source push mirror | the sender forge has one (Forgejo, GitLab) |
| 2 | destination pull mirror | the receiver can become a pull mirror (Forgejo only when the repository is created; never for an existing one) |
| 3 | `cfrg sync` | otherwise (Bitbucket or GitHub as sender): adapter-only, fast-forward-only Git copy |

`cfrg switch --capabilities` prints what each adapter declares:

| | Forgejo | GitLab Free | Bitbucket Free | GitHub (frozen) |
|---|---|---|---|---|
| push mirror (sender) | native | native | adapter-only (cfrg sync) | adapter-only (cfrg sync, read only) |
| pull mirror (receiver) | native at creation only | unsupported (Premium) | unsupported | unsupported |
| receiver lock | native + fill (user whitelist, every rule locked) | native + fill (deploy key only, default branch only: `docs/native-mirrors.md`) | native (push restrictions) | native ruleset on the default branch for native-mirror destinations (`docs/native-mirrors.md`); no switch |
| switch | native + fill | native + fill | native | unsupported |
| rename (follow the primary's name, `docs/native-mirrors.md`) | unsupported (names are made here) | native (project path) | native (name, the slug follows) | unsupported |

## Placement

One file, `lib/placement.json`. `cfrg switch` reads the whole file (or its
`native` view). The fields it adds to `native`, all optional so existing
consumers are unaffected:

* `receiver` `{mirror_user, password_env}`: the identity a sender writes as when
  Forgejo is a receiver, and the environment reference holding its credential.
  Mint that credential with the narrowest scope (`write:repository`).
* top-level `default` and `primaries` (already the git-pointer view) name the
  primary; `native.default` / `native.primaries` carry the same two fields when
  only the native view is generated. `cfrg native` and `cfrg serve` skip a
  repository whose primary is not the Forgejo source (`primary-elsewhere`).

The old primary comes from `--previous FILE` (the placement before the change,
for example `git show HEAD~1:lib/placement.json`), else `--from URL`, else the
primary the state file recorded after the last switch, else the default.

## The switch

```sh
# plan (default): reads only, prints the steps and the hash check
cfrg switch --placement lib/placement.json --previous old.json --state state.json --repository owner/repo
# act
cfrg switch ... --apply
```

| Step | What happens |
|---|---|
| a. hash check | the new primary holds every branch and tag of the old one at the same commit; no receiver holds a ref the new primary lacks (a mirror would delete it); a mirror between sites that cfrg does not own refuses. Replication that is merely behind is nudged and awaited (`--drain`) |
| c. freeze (early) | the old primary is locked against every writer, then the hash check runs once more on the frozen repository. If it fails the old primary is unlocked again and the switch refuses |
| b. disable | every owned mirror whose sender is not the new primary is removed (so two directions never exist at once) |
| d. unlock | the receiver lock on the new primary is removed for normal landing |
| e. create | per other site the mechanism above; for a push mirror the receiver admits only the mirror principal (this also protects the old primary as a receiver). `cfrg sync` edges are only declared; on GitLab the deploy keys of mirrors that no longer exist are removed once the rule names the new key |
| f. verify | the mirrors are forced, every site must show the new primary's heads, and only the new primary may own a mirror. The state file records the new primary |

The freeze precedes the disable on purpose: it is the only order in which no
write can slip in between the hash check and the change. Every step is
idempotent, so an interrupted run is simply run again with the same
arguments. Mirrors are owned by record: the state file keeps
`<sender origin>/<sender id>/<receiver origin>/<receiver path>` to the
mirror's id (the key `cfrg native` always used), and a declared `remote_name`
counts as a record. cfrg never removes a mirror it does not own.

## What the live probe showed (Forgejo 15, GitLab Free, Bitbucket Free)

* GitLab remote mirrors take credentials in the URL and do not percent-decode
  them: a `%3D` in a Bitbucket API token fails authentication, so they go in
  raw; credentials that would change the URL's structure are refused.
* GitLab Free locks a receiver to a deploy key only; a sender that cannot mint a
  mirror key (everything but Forgejo) cannot be admitted exclusively on a
  GitLab receiver. Forgejo's SSH port is not public, so a Forgejo receiver
  admits a user: with a single identity (the agent's own) the lock stops every
  other user, merges and, in the freeze, everyone, but not that identity.
* A Forgejo protection list is not paginated. The first matching rule wins,
  therefore the lock rewrites every rule and `unlock` restores only rules that
  carry the lock. `apply_to_admins` is set so the lock binds administrators.
* Bitbucket has no pipelines configuration until the first commit (404): there
  is nothing to switch off.
* Protected branches on every receiver refuse force-push and branch deletion:
  a rewritten history or a deleted branch on the primary shows up as a failing
  mirror, never as lost data on the receiver.
