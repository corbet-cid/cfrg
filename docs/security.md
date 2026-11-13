# Security model

## 1. Zero CI on copies

A copy never runs CI and never runs Dependabot. This is enforced by code, not by
convention.

| Guard | Evidence |
|---|---|
| GitHub: Actions and Dependabot alerts and security updates are switched off by every reconcile, with no way to keep them on | `src/native.rs` of the [cghb](https://git.corbet.ch/corbet-libre/cghb) repository (`disable_actions`, `disable_dependabot`); test `actions_and_dependabot_are_switched_off_with_no_way_to_keep_them_on` |
| GitLab pipelines and Bitbucket Pipelines are switched off on every destination | [native mirrors](native-mirrors.md); `src/native.rs` of the [cglb](https://git.corbet.ch/corbet-libre/cglb) repository, `src/native.rs` of the [cbkt](https://git.corbet.ch/corbet-libre/cbkt) repository |
| The placement has no CI field; a document carrying one is refused | `cfrg/tests/it/zero_ci.rs` (`no_destination_of_any_provider_can_declare_ci_on_or_off`, `one_declaration_of_ci_among_many_destinations_refuses_the_whole_placement`) |
| No CI executor outside our own compute can be modelled; provider-hosted and paid executors do not parse, and the own CI of a mirror forge is refused as a driver | [repository policy](repository-policy.md); `cfrg/tests/it/forge_policy.rs` |
| A fresh GitHub destination is created with Actions off and the first push waits for a verified off state | [gate](gate.md) "Quiet first push"; `cfrg-cli/tests/it/gate_create.rs` |
| Copies are receivers: only the mirror principal can write the default branch | [native mirrors](native-mirrors.md) |

Limit: Dependabot version updates follow `.github/dependabot.yml`, which a copy
carries because its primary does ([native mirrors](native-mirrors.md)).

## 2. Credentials

* Secrets come from environment variables named in configuration (`cfrg native`,
  `cfrg gate`...) or, for the long-running receiver and relay, from files of a
  credentials directory, never from the environment of the service
  ([configuration](configuration.md#credentials-directory)). Configuration files hold
  the name, never the value (`cfrg/src/native.rs`, `env_name`).
* The gate hands its token to git through `GIT_CONFIG_*` and to curl through
  `--variable`/`--expand-header`, never through arguments or files
  ([gate](gate.md) "Credential").
* The bridge gives each child process only its own forge's credential and
  follows no redirects, credential helpers or proxy settings
  ([contribution bridge](pr-bridge.md)).
* State files hold rate windows and intents, never credentials
  ([native mirrors](native-mirrors.md)).
* Webhook bodies are authenticated by HMAC-SHA256 over the raw body, compared in
  constant time, before anything is parsed: `cfrg/src/serve.rs` (the land lane) and `cfrg-serve/src/verify.rs` (the receiver)
  (`verify_signature`, `constant_time_eq`); tests `hmac_matches_the_rfc_4231_vectors`
  and `signatures_are_verified_exactly`.
* Least write scope: GitHub writes go through one gate with a kill switch, a
  pause per organisation on the first 403/429/402 answer, and a ledger written
  before each mutation ([gate](gate.md)).

## 3. Outside contributions and take-in after review

Two paths exist. The first is `cfrg bridge`, disabled unless `--enable-gitlab-import`, imports
a GitLab merge request as a WIP pull request on the primary. It checks out and
executes no code, hooks, filters or submodules; it checks that Actions are off
on both repositories and that the webhook inventories are empty, and it needs
`--confirm-ci-disabled` for the paths it cannot see. A WIP title is not a CI
gate ([contribution bridge](pr-bridge.md); `cfrg/src/bridge/tests.rs`).

The second is the relay ([relay](relay.md)). A pull or merge request from outside is
relayed as credited sibling requests on the copies and one `contribution` notice on the
primary, and nothing of it reaches the primary's branches or CI by itself
(`cfrg/src/relay/tests_contrib.rs`). The abuse screen holds new outside requests on
Forgejo by default (`screen: hold_all`) and a per-author daily cap bounds the rest.
After a maintainer's review, [`cfrg take-in`](take-in.md) fetches exactly the reviewed
commit, refuses if it moved since the review, and fast-forwards or merges it, never
rebasing or squashing, so authorship is preserved. Only after take-in does the
primary's CI run the commits (`cfrg-cli/src/take_in.rs` tests).

Receiver hardening (`cfrg-serve/src/server.rs`, `verify.rs`): one HMAC key per forge, so a
key that leaks from one provider signs nothing for another; the signature is checked over
the raw body before parsing; a listener answers only the routes in its table; a request
carries no authority, it names a repository and an object and the work re-reads everything
from the forge; a delivery id is remembered so a replay does nothing twice; bodies over 1 MiB
are refused; the relay has a kill switch file and per-forge pacing in the database
([relay](relay.md)).

## 4. Projects that are not ours

`Contributed::refuse` (`cfrg/src/contributed.rs`) stops mirror, relay and
release operations on declared contributed projects; the placement is invalid
when a contributed project has a live destination
([contributed](contributed.md)).

## Reporting a vulnerability

Open an issue on the primary repository and mark it as a security report; do not
post exploit details or credentials. A private reporting channel is not yet
established and is not claimed here.
