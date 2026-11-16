# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/); while it is below 1.0 a minor version may change behaviour.

## [0.2.0] - 2026-11-16

The first public release. The workspace carried the number `0.1.0` internally while it was private; no `0.1.0` was ever
published, so the first public version is `0.2.0`, a minor step because it adds the relay, the receiver and take-in and
changes the licence and some defaults.

### Added
- Issue, comment and pull or merge request relay between Forgejo, GitHub and GitLab (`cfrg relay apply`, `cfrg relay sweep`),
  on a PostgreSQL store with per-object leases, a journal written before every provider write, pacing across jobs, a
  kill switch and deletion caps ([relay](docs/relay.md)).
- Outside contributions: a credited sibling request on the other copies and one `contribution` notice on the primary, an
  abuse screen (`screen`: `hold_all` default, or `rate_limit`) and a per-author daily cap.
- `cfrg take-in`: takes exactly the reviewed commit of a contribution into the primary by fast-forward or merge commit,
  never by rebase or squash, and refuses a contribution that moved since the review ([take-in](docs/take-in.md)).
- `cfrg-serve`, the signed receiver: Forgejo, GitHub and GitLab events with one HMAC key per forge and replay protection,
  the relay in-process, release upload to GitHub and GitLab copies against a checksum contract, wiki push, and a hook runner for
  commands you supply ([configuration](docs/configuration.md), [release upload](docs/release-upload.md)).
- `cfrg daily`: one run for a scheduler (drift reconcile, copy settings, wiki push, relay sweep, key
  renewal hook), each step alone.
- Native mirror management beyond push mirrors: copy settings (visibility, description, topics, default branch, links back
  to the primary, CI and Dependabot off), a member plan that never grants write, a default-branch protection check, wiki push, and
  mirror ownership derived from Forgejo's own mirror list instead of recorded state ([native mirrors](docs/native-mirrors.md)).
- A Nix flake: packages `cfrg`, `cfrg-serve` and `cfrg-serve-image` (a container image), and a NixOS module
  ([deployment](docs/deployment.md)).
- Documentation for publication: README, concepts, configuration reference, deployment, security model, a quick start that
  runs end to end ([quick start](docs/quickstart.md)), CONTRIBUTING with the AI-assistance disclosure.

### Changed
- The four drivers `cfgj`, `cghb`, `cglb` and `cbkt` moved out of the workspace into their own public repositories
  and are depended on by pinned git revision.
- Licence changed from FSL-1.1-ALv2 to LGPL-3.0-only WITH LGPL-3.0-linking-exception. Licence texts in `LICENSES/`,
  per-file declarations in `REUSE.toml`, `license` field in every `Cargo.toml`.
- `cfrg native --operation members` requires `--operator LOGIN` (the login that is never touched); it no longer has a default.
- `cfrg take-in` requires `--forge`, `--primary-host` and `--reviewer`, reads its Forgejo token from `CFRG_FORGEJO_TOKEN`
  only, and commits with the git identity you have configured. Nothing is preset.
- `cfrg-serve`: `forge_hosts` defaults to the host of `forge_web` instead of a preset host name.

- Native API: an HTTP 403 that is not a rate limit (no `Retry-After`, `x-ratelimit-remaining` not 0) refuses that one request
  instead of stopping the provider for the run; three such refusals in a row on one scope, without a success in between (a
  flagged or suspended account), still end the run. `settings` reports a refused write as a `drift` row naming the fields and
  wanted values, not as an error. The `derive.exclude` patterns now also hold for the `settings` rows of every provider.

- `cfrg gate`: `budgets.new_repos_per_day_by_org` gives single organisations their own daily creation cap (a ramp for a
  fresh GitHub App installation); an organisation without an entry keeps `new_repos_per_day_org`.

- `cfrg native --operation members --apply`: writes the planned GitHub grants (triage only, paced, `--max-writes` per run, never lowering a role),
  with the token in `CFRG_GITHUB_MEMBERS_TOKEN`.

- `cfrg gate`: every command that decides on the ledger or the queue (flush, create, dispatch, delete, enqueue) now holds an
  exclusive lock on the state directory while it loads and uses them. Before, the receiver's hook and a hand call could decide
  on the same stale snapshot and together exceed a budget (66 pushes against 60 a day, 18 against 12 an hour).

- `cfrg native`: creating a push mirror for an ARCHIVED Forgejo primary (HTTP 423) lifts the archive for that one call and puts
  it back, also when the creation fails.

### Not provided
- Prebuilt release binaries. Build from the flake or from source ([deployment](docs/deployment.md)).
- Writing member roles to copies: `cfrg native --operation members --apply` is refused; only the plan exists.
- A private channel for vulnerability reports; use the issue tracker ([security](docs/security.md)).

### Earlier, internal
Native push mirrors, the GitHub write gate, `land`, `serve` (the land lane), `release`, `switch`, access sync, `collect`,
`contents`, `observe`, `status` and the experimental GitLab contribution bridge existed before this release; they are
described in the pages of `docs/`.
