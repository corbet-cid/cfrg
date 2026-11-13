# Concepts

| Term | Meaning | Where it is defined |
|---|---|---|
| Primary | The Forgejo repository where work happens. The only place code is written and CI runs. | [repository policy](repository-policy.md) |
| Copy (destination) | The same repository on GitHub, GitLab or Bitbucket. A receiver: only the mirror principal may write its default branch. | [native mirrors](native-mirrors.md) |
| Placement | The one declared document that lists each repository, its Forgejo `source_id` and its destinations. | [native mirrors](native-mirrors.md), `cfrg/src/placement.rs` |
| Driver | A crate that speaks one forge's API: `cfgj` (Forgejo), `cghb` (GitHub), `cglb` (GitLab), `cbkt` (Bitbucket). | the workspace `Cargo.toml` members |
| Native first | If Forgejo has a mechanism (push mirror, merge when checks succeed), cfrg configures it instead of reimplementing it. | [land](land.md) "Native first"; [native mirrors](native-mirrors.md) |
| Gate | The single writer to GitHub, with budgets, a kill switch and an append-only ledger. | [gate](gate.md) |
| Contributed project | Someone else's project we contribute to. Never mirrored, never relayed, no CI run by us. | [contributed](contributed.md); `cfrg/src/contributed.rs` |
| Hold | A declared pause on a destination: owned mirrors are removed so scheduled retries cannot bypass it. | [native mirrors](native-mirrors.md) |

## Design rules

* Forgejo is the platform; cfrg fills only gaps. Mirror ownership is derived from
  Forgejo's own mirror list plus the declaration: a push mirror is owned exactly
  when its remote address equals a declared destination (or it carries a declared
  `remote_name`). Nothing is recorded, so a lost state directory cannot break it;
  every other push mirror is foreign, reported as `foreign` and never touched.
* Omission is not deletion. A mirror is removed only when the placement says
  `absent: true`, and a destination repository is never deleted
  ([native mirrors](native-mirrors.md)).
* Reads before writes. Plans and status are read-only; every change needs
  `--apply` or `--execute`.
* Pacing. Provider writes go through budgets with durable windows, so a burst of
  work on the primary cannot become a burst on a copy ([gate](gate.md)).
* One repository per library. The four drivers are separate crates; planned to
  become separate repositories (B15).

## Planned relay

Relaying issues, comments and pull requests between forges (`cfrg relay`) is
planned (B2, B3, B8). The mapping of linked objects will live in one small
PostgreSQL table (B1). None of this exists in the code yet.
