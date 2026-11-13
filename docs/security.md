# Security model

## 1. Zero CI on copies

A copy never runs CI and never runs Dependabot. This is enforced by code, not by
convention.

| Guard | Evidence |
|---|---|
| GitHub: Actions and Dependabot alerts and security updates are switched off by every reconcile, with no way to keep them on | `cghb/src/native.rs` (`disable_actions`, `disable_dependabot`); test `actions_and_dependabot_are_switched_off_with_no_way_to_keep_them_on` |
| GitLab pipelines and Bitbucket Pipelines are switched off on every destination | [native mirrors](native-mirrors.md); `cglb/src/native.rs`, `cbkt/src/native.rs` |
| The placement has no CI field; a document carrying one is refused | `cfrg/tests/it/zero_ci.rs` (`no_destination_of_any_provider_can_declare_ci_on_or_off`, `one_declaration_of_ci_among_many_destinations_refuses_the_whole_placement`) |
| No CI executor outside our own compute can be modelled; provider-hosted and paid executors do not parse, and the own CI of a mirror forge is refused as a driver | [repository policy](repository-policy.md); `cfrg/tests/it/forge_policy.rs` |
| A fresh GitHub destination is created with Actions off and the first push waits for a verified off state | [gate](gate.md) "Quiet first push"; `cfrg-cli/tests/it/gate_create.rs` |
| Copies are receivers: only the mirror principal can write the default branch | [native mirrors](native-mirrors.md) |

Limit: Dependabot version updates follow `.github/dependabot.yml`, which a copy
carries because its primary does ([native mirrors](native-mirrors.md)).

## 2. Credentials

* Secrets come from environment variables named in configuration. Files hold
  the name, never the value (`cfrg/src/native.rs`, `env_name`).
* The gate hands its token to git through `GIT_CONFIG_*` and to curl through
  `--variable`/`--expand-header`, never through arguments or files
  ([gate](gate.md) "Credential").
* The bridge gives each child process only its own forge's credential and
  follows no redirects, credential helpers or proxy settings
  ([contribution bridge](pr-bridge.md)).
* State files hold rate windows, intents and ownership IDs, never credentials
  ([native mirrors](native-mirrors.md)).
* Webhook bodies are authenticated by HMAC-SHA256 over the raw body, compared in
  constant time, before anything is parsed: `cfrg/src/serve.rs`
  (`verify_signature`, `constant_time_eq`); tests `hmac_matches_the_rfc_4231_vectors`
  and `signatures_are_verified_exactly`.
* Least write scope: GitHub writes go through one gate with a kill switch, a
  pause per organisation on the first 403/429/402 answer, and a ledger written
  before each mutation ([gate](gate.md)).

## 3. Outside contributions and take-in after review

Today (built): `cfrg bridge`, disabled unless `--enable-gitlab-import`, imports
a GitLab merge request as a WIP pull request on the primary. It checks out and
executes no code, hooks, filters or submodules; it checks that Actions are off
on both repositories and that the webhook inventories are empty, and it needs
`--confirm-ci-disabled` for the paths it cannot see. A WIP title is not a CI
gate ([contribution bridge](pr-bridge.md); `cfrg/src/bridge/tests.rs`).

Planned (B8, B13): outside commits are held out of the primary until a human
maintainer has had them reviewed; `cfrg take-in` will fetch exactly the reviewed
commit and fast-forward or merge it, never rebase or squash, so authorship is
preserved. Only after take-in would the primary's CI run it. None of this
exists in the code yet.

## 4. Projects that are not ours

`Contributed::refuse` (`cfrg/src/contributed.rs`) stops mirror, relay and
release operations on declared contributed projects; the placement is invalid
when a contributed project has a live destination
([contributed](contributed.md)).

## Reporting a vulnerability

Open an issue on the primary repository and mark it as a security report; do not
post exploit details or credentials. A private reporting channel is not yet
established and is not claimed here.
