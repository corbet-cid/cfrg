# Landing

`cfrg land` makes sure only an exact green commit reaches the default branch.
The agent that wants a change landed pushes its branch once, runs one command
and is done.

```sh
cfrg land --policy land-policy.json --state-dir "$STATE" corbet-cid/cfrg ci/my-change
```

The command puts the branch in the repository's queue, runs one pass (a branch
that is already green and current lands right away), starts a detached
follower when something is still waiting and returns. The follower repeats the
same pass every `interval_seconds` until the queue is settled; `cfrg land
--step` runs one pass for a timer or for a future `cfrg serve`.

## Contract

| Rule | How it holds |
|---|---|
| Only the exact green commit lands | cfrg merges (fast-forward-only, naming `head_commit_id`) only a head it saw succeed for every declared context, re-reads the pull request right before the merge and refuses when the head moved; a new push resets the wait |
| One queue per repository | the open pull requests carrying the cfrg marker, oldest first; only the head-of-line entry may land or be rebased |
| Base moved | the forge rebases the branch (`pulls/{n}/update?style=rebase`), the merge is scheduled again for the new head and a retest is requested; it lands only after the new head is green |
| Required context missing | the head-of-line entry whose head lacks a required context gets the `retest` command once (the policy's own gate; twice when it has no status at all). If the context is still missing after `gate_timeout_seconds` (default 2700, per repository override) the entry is commented with the reason, closed and skipped so the queue continues; nothing is merged without the gating contexts green on the exact head |
| Conflict | the entry is closed with an explanation; push a rebased branch and land again |
| Red head | skipped, never blocks the queue; a new push to the branch makes it live again |
| No agent after the push | the follower needs only the policy file and the token variable |

## Native first (Forgejo 15, verified live)

| Step | Native mechanism | cfrg fills |
|---|---|---|
| Review unit | pull request | the marker that makes it a queue entry |
| Merge when green | `merge_when_checks_succeed` with `Do=fast-forward-only` (201), ONLY where the default branch is status-gated for the declared contexts | on a branch without that protection Forgejo treats "no required checks" as success and merges a head that has no status at all (seen live: cmsh PR 1, merged on a push before any CI status existed). cfrg therefore asks the forge whether the rule exists (`gated`); where it does not, it never schedules the native merge, cancels one an older cfrg scheduled (`native-merge-cancelled`) and merges itself after observing the status. Where it does, the native path fires on the next success status and cfrg still merges green heads itself |
| Linear history | `Do=fast-forward-only` | a stale head would answer 500, so cfrg checks the fresh tip first |
| Rebase | `POST pulls/{n}/update?style=rebase` (200, 409 on conflict) | queue order and the retest after it |
| Gate | branch protection with `status_check_contexts` (glob patterns) | declared and reconciled by `cfrg land --protect` |
| Retest | none (manual-event CI) | `retest` command, or CI that starts from the forge's own events |

The status gate applies to pull request merges only. A user who may push can
still push to the protected branch directly: enforcement is SOFT and `cfrg land
--protect` never restricts pushes. (Julian's decision on strictness is pending;
nothing stricter exists.) Forgejo 15 protected branches always refuse force
pushes and branch deletion.

## Policy file (JSON, declared in a repository)

```json
{
  "schema": 1,
  "forge": "forgejo",
  "endpoint": {"origin": "https://forge.corbet.ch", "token_env": "CFRG_LAND_TOKEN"},
  "contexts": ["ci/*"],
  "interval_seconds": 30,
  "follow_seconds": 14400,
  "retest": ["ci-job", "run", "--repo", "{checkout}", "--branch", "{branch}", "--job", "verify"],
  "landed": ["ci-job", "run", "--repo", "{checkout}", "--branch", "{branch}", "--job", "release"],
  "repositories": [{"path": "corbet-cid/cfrg", "contexts": ["ci/crow/*"]}]
}
```

* `contexts`: status context patterns (`*` wildcard); each must match at least
  one status and every match must be `success` for the exact head. Overridable
  per repository, together with `retest`.
* `retest`: optional command asked for a CI run of an exact commit when the
  head has no status yet and after every rebase (at most twice per commit).
  Leave it out when CI starts from the forge's own push events.
* `landed`: optional command started when an exact commit has reached the
  default branch, whether cfrg merged it or the forge did on its own through
  the scheduled merge (for example the job that publishes the release).
* Placeholders in both: `{repository}`, `{branch}`, `{sha}`, `{origin}` and
  `{checkout}`. `{checkout}` is a work clone of the repository that cfrg
  prepares at exactly that commit (fetched with the token from its
  environment, never from argv). Hook commands run to completion in order
  inside a pass, with a ten-minute bound, and get the environment without the
  token variable; they should only submit work.
* The token needs repository write (pull requests, merges) and, for
  `--protect`, repository admin.

## Modes

| Command | What it does |
|---|---|
| `cfrg land REPO BRANCH` | enqueue, one pass, start the follower, return |
| `cfrg land --step [REPO]` | one idempotent pass (all declared repositories without `REPO`) |
| `cfrg land --status [REPO]` | read-only queue with each head's verdict |
| `cfrg land --protect [--apply] [REPO]` | plan or reconcile the status gate on the default branch |
| `cfrg land --capabilities` | what each forge adapter declares (`native`, `native-fill`, `adapter-only`, `unsupported`) |

With `"follow": "serve"` in the policy, `cfrg land REPO BRANCH` only enqueues and
schedules the native merge; `cfrg serve` (`docs/serve.md`) reacts to the forge's
events and finishes the landing, so no follower is started.

State is one directory: request windows and pacing (`http.json`), the journal
of one-shot effects (`journal.json`), hook output (`hooks.log`), work clones
(`work/`) and follower logs. A request state lock
whose owner process is gone is cleared; a recorded uncertain write is resolved
by reading the pull request back.

## Other forges

Only Forgejo is implemented; every other adapter answers `unsupported` rather
than guessing (see `--capabilities`).

## Forbidden ancestors (history rewrites)

After a history rewrite, a branch based on the old history must never be
rebased onto the new default branch (the forge would replay the whole old
history). The land policy therefore declares commits no head may contain, data
only:

```json
{
  "forbidden_ancestors": ["<40 hex old root>"],
  "forbidden_ancestors_file": "/etc/cfrg/forbidden-ancestors.json",
  "repositories": [{"path": "org/repo", "forbidden_ancestors": ["<40 hex>"]}]
}
```

The optional file is re-read on every pass (no restart, no code change):

```json
{"schema": 1, "forbidden_ancestors": {"org/repo": ["<40 hex>"], "*": ["<40 hex>"]}}
```

`*` applies to every repository. The three sources are merged. An unreadable
or malformed file stops all landing (fail closed). With no commits declared
the guard does nothing and makes no request.

The check runs when the request is enqueued, for every queued head before its
statuses are read (so before any scheduling or rebase), and again right before
the merge. A head whose history contains a forbidden commit is refused: the
scheduled native merge is cancelled, the pull request gets a comment ("based
on pre-rewrite history; translate the branch with the published map") and is
closed, the event `blocked` with `reason: forbidden-ancestor` is reported, and
the queue moves on. On Forgejo the ancestry test is
`GET commits?sha=<forbidden>&not=<head>&limit=1`: an empty list means the
forbidden commit is the head or its ancestor; a 404 (the forge no longer has
that commit) means no head it holds can contain it.

## Signed heads: fast-forward landings only

A rebase or re-commit destroys commit signatures. A repository that needs the
author's signature to survive declares, per repository, data only:

```json
{"repositories": [
  {"path": "org/repo", "rebase": "never"},
  {"path": "org/other", "no_rebase_paths": ["secrets/"]}
]}
```

`rebase: "never"` covers every head of the repository; `no_rebase_paths`
(files, or directory prefixes matched by path component, so `secrets/` does not
match `secrets-x/` or `docs/secrets/`) covers only heads that touch one of the
paths. Absent: unchanged behaviour (the forge rebases a head whose base moved).

A covered head lands only as a pure fast-forward of the default branch: cfrg
merges with `Do=fast-forward-only` (the forge moves the ref, no new commit, no
merge commit, no squash; every landing path in cfrg uses this style). If the
base moved, cfrg never calls the forge's rebase: it cancels the scheduled
merge, comments "this repository requires signed fast-forward landings: rebase
locally, re-sign, push again", closes the pull request, reports `blocked` with
`reason: rebase-forbidden`, and the queue moves on. Whether a head touches a
path is read from `GET commits?sha=<head>&not=<default>&files=true` (every
commit's own file list, so a later revert does not hide a signed change) plus
the net `pulls/<n>/files`; an incomplete answer counts as touching (fail
closed). Heads not touching the paths still rebase normally.

