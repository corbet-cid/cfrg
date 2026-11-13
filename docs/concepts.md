# Concepts

| Term | Meaning | Where it is defined |
|---|---|---|
| Primary | The Forgejo repository where work happens. The only place code is written and CI runs. | [repository policy](repository-policy.md) |
| Copy (destination) | The same repository on GitHub, GitLab or Bitbucket. A receiver: only the mirror principal may write its default branch. | [native mirrors](native-mirrors.md) |
| Placement | The one declared document that lists each repository, its Forgejo `source_id` and its destinations. | [native mirrors](native-mirrors.md), `cfrg/src/placement.rs` |
| Driver | A crate that speaks one forge's API: `cfgj` (Forgejo), `cghb` (GitHub), `cglb` (GitLab), `cbkt` (Bitbucket). | the `[workspace.dependencies]` of `Cargo.toml`; [the driver repositories](../README.md#the-driver-repositories) |
| Native first | If Forgejo has a mechanism (push mirror, merge when checks succeed), cfrg configures it instead of reimplementing it. | [land](land.md) "Native first"; [native mirrors](native-mirrors.md) |
| Gate | The single writer to GitHub, with budgets, a kill switch and an append-only ledger. | [gate](gate.md) |
| Contributed project | Someone else's project we contribute to. Never mirrored, never relayed, no CI run by us. | [contributed](contributed.md); `cfrg/src/contributed.rs` |
| Hold | A declared pause: nothing about the held repository or destination is touched, its mirrors included. | [native mirrors](native-mirrors.md) |

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
* One repository per library. The four drivers are separate repositories under
  [corbet-libre](https://git.corbet.ch/corbet-libre), pinned by revision in the workspace `Cargo.toml`
  ([README](../README.md#the-driver-repositories)).

## Relay

The relay keeps issues, comments and pull or merge requests in step across Forgejo, GitHub and GitLab. Forgejo is the
hub: it is written first and its object is the reference. Every write is journalled in a PostgreSQL table before it is
made and the single writer of a logical object holds a lease, so a repeated or concurrent event writes nothing twice
([relay](relay.md), `cfrg/src/relay/`). Outside pull and merge requests become credited sibling requests plus one
notice on the primary, and reach the primary only through [take-in](take-in.md) after a human review.

## Receiver

`cfrg-serve` is the one long-running service that reacts to events: it verifies a signature per forge, answers fast,
and then runs the relay, the release upload ([release upload](release-upload.md)) or the wiki push in-process. It has no
queue of its own beyond a bounded in-memory channel and no timer: one attempt per event, and the next event
completes what stopped ([configuration](configuration.md#serve-configuration-cfrg-serve)).
