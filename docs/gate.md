# gate

`cfrg gate` is the one place that writes to GitHub: it pushes the newest state of a repository from a local source (the git copy), under budgets, so a burst of
work on the primary forge can never become a burst of pushes on GitHub.

```sh
cfrg gate --config gate.json status
cfrg gate --config gate.json enqueue org/repo ...
cfrg gate --config gate.json create org/repo --private --description "..."   # dry run
cfrg gate --config gate.json flush              # dry run: prints what it would push
CFRG_GITHUB_TOKEN=... cfrg gate --config gate.json flush --execute
cfrg gate --config gate.json dispatch org/repo ccid-remote.yml main --input job=macos --input sha=<40 hex>   # dry run
CFRG_GITHUB_TOKEN=... cfrg gate --config gate.json dispatch org/repo ccid-remote.yml main --input k=v --execute
```

| Rule | Behaviour |
|---|---|
| Queue | holds repository names, never commits; queueing twice keeps the first time; a flush pushes the newest state of every branch and tag once (N commits, one push) and skips a repository whose heads already equal GitHub's |
| Budgets | defaults: 20 pushes per hour and 120 per day overall, 8 per hour per organisation, 10 minutes between pushes to one repository, 3 new repositories per day per organisation and 6 overall, 30 Actions runs per day. Any of them may be overridden under `budgets` |
| Lanes | a repository with `lane: "build"` (its GitHub Actions must build current code) goes first, may use the reserved slice (`build_reserve_per_hour` 6, `build_reserve_per_day` 30) that the bulk lane cannot, and every push to it is counted as an Actions run |
| Ledger | `state_dir/ledger.jsonl`, append-only; a mutation is recorded before it is made, so a crash can only over-count |
| Pause | `paused_dir/<org>` exists = that organisation is refused (written by a flag probe, or by the gate itself on the first 403, 429, 402 or abuse answer; never removed by the gate) |
| Kill switch | `kill_switch` directory exists = nothing is written, for every organisation |
| Creation | `create` makes one listed repository in its organisation under the same budgets: the per-organisation and overall daily caps, the overall `create_min_gap_secs` (default 300) between two creations, the pause, the kill switch and the ledger written before the POST. Visibility and description come from the caller (copied from the primary, never changed later); the repository is created with issues, projects and wiki off. It refuses an existing repository instead of spending the creation. A missing destination in `flush` now points here |
| Dispatch | starts one provider workflow (`workflow_dispatch`) on a `lane: "build"` repository only: the daily Actions-run budget, kill switch and paused organisation apply, the ledger entry is written before the first call, Actions are switched on once if they are off, and at most 10 `key=value` inputs are accepted (key `[A-Za-z0-9_-]+`, value at most 200 characters without control characters) |
| Delete | `delete-repo org/repo --allowlist FILE` deletes ONE old private repository. The allowlist (`{"repos": {"org/repo": {"head": "<40-hex>"}}}`, strict) names every deletable repository with the commit its default branch must still be at. Refused: an unlisted repository, a public one, a moved head, a kill switch, a paused organisation, and any deletion within `delete_min_gap_secs` (default 600, overall) of the previous one or beyond `deletes_per_day` (default 6). Both GitHub reads (private, head) run in the dry run too and need the credential; the ledger entry is written before the DELETE call. The dry run prints what would be deleted |
| Credential | read from the environment variable named by `--token-env` at the last moment and handed to git through `GIT_CONFIG_*` (an `http.extraheader`) or to curl through `--variable`/`--expand-header` for `create` and `dispatch`, never through arguments or files; `--origin`, `--api` exist for tests |

The configuration is JSON: `budgets` (optional), `repos` (`org/repo` to `{lane, source}`; a repository that is not listed is never pushed), `state_dir`, `paused_dir`,
`kill_switch`. The pure decisions are `cfrg::gate`; `cfrg-cli/tests/gate_flush.rs` runs the executor against local bare repositories, `cfrg-cli/tests/gate_create.rs` covers the creation budget, gap and validation without network, and
`cfrg-cli/tests/gate_dispatch.rs` covers dispatch validation and refusals without network access; `cfrg-cli/tests/gate_delete.rs` and the unit tests in `cfrg-cli/src/delete.rs` cover the deletion refusals.
