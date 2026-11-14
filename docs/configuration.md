# Configuration reference

cfrg reads JSON or TOML files named on the command line. Secrets are never in a
file: a placement names an environment variable (`token_env`, `password_env`) and
cfrg reads the value at the last moment (`cfrg/src/native.rs` validates the
variable name with `env_name`); the long-running services read credentials from
files of a credentials directory instead.

| File | Command | Where |
|---|---|---|
| serve configuration and credentials directory | `cfrg-serve serve` | [Serve configuration](#serve-configuration-cfrg-serve) |
| relay configuration | `cfrg relay`, `cfrg-serve serve`, `cfrg daily` | [Relay configuration](#relay-configuration) |
| placement (`--placement`) | `cfrg native`, `cfrg relay`, `cfrg-serve serve` | [Placement fields](#placement-fields), [native mirrors](native-mirrors.md) |
| state (`--state`) | `cfrg native` | persistent private storage with rate windows, pacing, uncertain intents (no ownership: it is derived from Forgejo); never credentials ([native mirrors](native-mirrors.md)) |
| daily credentials directory | `cfrg daily` | [Daily run](#daily-run-cfrg-daily) |
| repository policy (`--policy`) | `cfrg validate`, `plan`, `decide`, `clone`, `sync` | [repository policy](repository-policy.md); example `cfrg/examples/repository-policy.json` |
| landing policy (`--policy`, with an optional `serve` block) | `cfrg land`, `cfrg serve` | [land](land.md), [serve](serve.md) |
| gate configuration (`--config`) | `cfrg gate` | [gate](gate.md) |
| release configuration (`--config`) | `cfrg release` | [release](release.md) |
| bridge configuration (`--config`, TOML) | `cfrg bridge` | [contribution bridge](pr-bridge.md) |
| identities (`--identities`, TOML) | `cfrg access`, `cfrg native --operation members` | [access sync](access-sync.md); example `docs/examples/identities.toml` |
| status configuration (`--config`) | `cfrg status` | [native status](native-status.md) |

## Minimal placement

The smallest useful file: one repository, one GitLab copy
(`docs/native-mirrors.md`, which carries the field-by-field explanation).

```json
{
  "schema": 1,
  "source": {"origin": "https://forge.example", "token_env": "FORGE_TOKEN"},
  "repositories": [{
    "path": "team/project", "source_id": 123, "private": false,
    "default_branch": "main", "content": "native-git", "hold": null,
    "destinations": [{
      "provider": "gitlab",
      "endpoint": {"origin": "https://gitlab.example", "token_env": "GITLAB_TOKEN"},
      "path": "team/project", "namespace": "42", "repository_id": "456",
      "mirror_user": "789", "password_env": "GITLAB_TOKEN", "use_ssh": true,
      "interval_seconds": 3600, "branch_filter": "", "hold": null,
      "absent": false, "remote_name": null
    }]
  }]
}
```

Strictness: unknown fields are refused when a file loads (`deny_unknown_fields`),
so a file that still carries the removed `"ci"` field on a destination is
rejected, and no declaration can keep a copy's CI on
(`cfrg/tests/it/zero_ci.rs`).

## Serve configuration (`cfrg-serve`)

`cfrg-serve serve` reads one JSON file, named by `FORGE_BRIDGE_CONFIG` (default `/config/config.json`), and a
credentials directory, named by `FORGE_BRIDGE_SECRETS` (default `/secrets`). Unknown fields are rejected
(`#[serde(deny_unknown_fields)]` in `cfrg-serve/src/config.rs`). Code: `cfrg-serve/src/config.rs`.

| Field | Required | Meaning |
|---|---|---|
| `listeners` | yes, to listen at all | Map of listener name to `{listen, port, tls_dir, enabled}`. Only the names `internal` and `public` have routes (table below). `listen` defaults to `0.0.0.0`, `port` to `0`, `enabled` to `true`. `tls_dir` names a directory with `tls.crt` and `tls.key`; the listener then speaks TLS 1.3 only. |
| `forge_api` | yes | Forgejo API base, e.g. `https://forge.example.org/api/v1`. |
| `forge_web` | yes | Forgejo web origin, e.g. `https://forge.example.org`. |
| `forge_hosts` | no | The spellings of the primary forge (the first is its web host). Default: the host of `forge_web`. |
| `forge_ca` | no | PEM bundle that is the only trust anchor for the Forgejo client. Absent: the public roots. The providers always use the public roots. |
| `github` | yes | `{api, uploads, web, min_write_interval_s, orgs}`: API and upload bases, the minimum seconds between writes (default `3.0`), and the organisations enabled for release upload. An empty `orgs` list disables the GitHub upload. |
| `gitlab` | yes | `{api, groups}`: the API base (e.g. `https://gitlab.com/api/v4`) and the top-level groups (or users) enabled for release upload. |
| `aliases` | no | `{provider: {primary path: copy path}}` for the few copies that carry another name. |
| `placement` | no | Path of the placement document. It declares the contributed projects (never touched) and, for the wiki leg, the repositories. |
| `wiki` | no | `{state, key, known_hosts, key_id}`: the state directory, the SSH deploy key (private half; `<key>.pub` beside it), the `known_hosts` file and the optional id of the deploy key on GitLab. Absent: a wiki event is answered 503. |
| `hook_commands` | no | `{copy, downsync, lockrefresh, gate, handoff, enroll}`: the argv of the command a hook runs; the job's arguments are appended. A hook whose key is absent answers 503. cfrg ships none of these commands. |
| `relay` | no | `{config, placement, credentials_dir, database_ca}`: the in-process relay. `config` is the relay configuration (next section), `placement` the placement it derives its repositories from, `credentials_dir` defaults to `/credentials`, `database_ca` is the CA file the database login is verified against. Absent: the relay routes answer 503. |
| `replay` | no | `{ttl_seconds, capacity}`: how long (default 86400) and how many (default 10000) delivery ids are remembered. |

Routes (`cfrg-serve/src/server.rs`, function `route`). A listener answers nothing that is not in its rows.

| Listener | Method and path | Signature key | Effect |
|---|---|---|---|
| `internal` | `POST /hooks/release` | `hmac_forgejo` | A published, non-draft release is uploaded to the GitHub and GitLab copies ([release upload](release-upload.md)). |
| `internal` | `POST /hooks/sync` | `hmac_forgejo` | `{"repository": "OWNER/REPO", "tag": "vX"}`: a tag pusher's hint that the tag is on a copy now. |
| `internal` | `POST /hooks/forgejo` | `hmac_forgejo` | Issues, comments and pull requests of the primary go to the relay; a `wiki` event starts the wiki push. |
| `internal` | `POST /hooks/copy`, `/hooks/downsync`, `/hooks/enroll`, `/hooks/gate`, `/hooks/handoff`, `/hooks/lockrefresh` | `hmac_forgejo` | Run the matching `hook_commands` entry. |
| `public` | `POST /hooks/github` | `hmac_github` | GitHub issue, comment and pull request events go to the relay. |
| `public` | `POST /hooks/gitlab` | `hmac_gitlab` | The same for GitLab (Standard Webhooks signature). |
| both | `GET /healthz` | none | Liveness. |

Signatures are checked over the raw body before anything is parsed (`cfrg-serve/src/verify.rs`): Forgejo sends a hex
HMAC-SHA256 in `X-Forgejo-Signature` (`X-Gitea-Signature` and `X-Hub-Signature-256` are also accepted), GitHub
`X-Hub-Signature-256: sha256=<hex>`, GitLab `webhook-signature: v1,<base64>` over `id.timestamp.body` with the
key `whsec_<base64>` and a five-minute timestamp window. There is one key per forge, so a key that leaks from one
provider signs nothing for another. A body over 1 MiB is refused (413), a bad signature answers 401, a repeated
delivery id answers 200 `duplicate`.

### Credentials directory

One file per key; the value is the file content with surrounding whitespace removed; an empty or missing file means
the key is absent. Never an environment variable (`CREDENTIAL_KEYS` in `cfrg-serve/src/config.rs`).

| File | Used for |
|---|---|
| `hmac_forgejo` | verifies `/hooks/release`, `/hooks/sync`, `/hooks/forgejo` and the command hooks |
| `hmac_github` | verifies `/hooks/github` |
| `hmac_gitlab` | verifies `/hooks/gitlab` (the signing token, `whsec_...`) |
| `forgejo-token` | reads the release on the primary (optional for a public repository) |
| `gitlab-token` | release upload to GitLab; the relay's GitLab adapter |
| `github-token` | release upload to GitHub |
| `github-app-id`, `github-app-key` | the relay's GitHub App (a decimal id and the PEM private key); the installation of each repository owner is resolved with the App JWT |
| `wiki-forgejo-token` | reads the wiki repositories on the primary |
| `host`, `port`, `dbname`, `username`, `password` | the relay database (`relay.credentials_dir`); the connection is `sslmode=verify-full` against `relay.database_ca` |

## Relay configuration

A JSON file named by `relay.config` (serve) or `--config` / `CFRG_RELAY_CONFIG` (`cfrg relay`). Parser:
`RouterConfig` in `cfrg/src/relay/model.rs`; unknown fields are rejected. The repository set is derived from the
placement; a `repos` list in this file is refused (`cfrg-relay/src/lib.rs`).

| Field | Default | Meaning |
|---|---|---|
| `relay` | required | `{forge: [user ids]}` with the forge names `forgejo`, `github`, `gitlab`: the immutable user ids of the account the relay acts as on each forge. A delivery whose actor is one of these ids is dropped. While a forge has members but no id, the run refuses. |
| `people` | none | Identity table: `[{key, name, handles: {forge: login}}]`, used to credit authors. |
| `pacing` | `namespace_per_hour` 60, `namespace_per_day` 400, `write_gap_secs` 2, `create_gap_secs` 12 | Caps and minimum gaps per forge namespace, shared by every job through the database. |
| `limits` | `author_fanout_per_day` 5, `object_write_passes_per_hour` 30, `cascade_deletes_per_hour` 10, `cascade_deletes_per_repo_per_hour` 3, `lease_secs` 600, `sweep_older_than_secs` 3600, `sweep_max_objects` 50, `max_new_comments_per_pass` 100 | See [relay](relay.md#limits). |
| `kill_switch` | none | A path. While it exists the relay makes no request at all, reads included. |
| `held_label` | `held` | The label that marks an object held on Forgejo. It is never relayed. |
| `local_labels` | `["held", "contribution"]` | Labels that exist on one forge only and are never merged across forges. |
| `screen` | `hold_all` | The abuse screen for outside contributions: `hold_all` keeps every new outside request on Forgejo for a human; `rate_limit` lets requests through and relies on `limits.author_fanout_per_day` as the only brake. |

```json
{
  "relay": {"forgejo": ["1"], "gitlab": ["2001"], "github": ["3001"]},
  "pacing": {"write_gap_secs": 2, "create_gap_secs": 12},
  "limits": {"author_fanout_per_day": 5},
  "screen": "hold_all"
}
```

## Placement fields

The placement is the document of `cfrg native`; `cfrg-serve` and the relay read the same file. It may be the
object itself or wrapped as `{"native": {...}}` (`Placement::from_document`, `cfrg/src/native.rs`). Field-by-field
explanations: [native mirrors](native-mirrors.md).

| Level | Field | Meaning |
|---|---|---|
| top | `schema` | `1` |
| top | `source` | `{origin, token_env}`: the Forgejo origin and the name of the environment variable that holds its token |
| top | `repositories` | the list below |
| top | `copy` | optional plain git host over SSH that receives every repository the source lists |
| top | `receiver` | optional `{mirror_user, password_env}` for a primary switch |
| top | `default`, `primaries` | the git pointer view; exceptions per repository |
| top | `contributed` | `[{name, upstream, forks}]`: projects that are not ours; never mirrored, relayed or released |
| repository | `path`, `source_id`, `private`, `default_branch` | the primary: its `org/repo`, its immutable Forgejo id, visibility, default branch |
| repository | `content` | `native-git`, `lfs` or `profile` |
| repository | `hold` | a reason string pauses everything about the repository; `null` otherwise |
| repository | `destinations` | the copies |
| destination | `provider` | `gitlab`, `bitbucket` or `github` |
| destination | `endpoint` | `{origin, token_env}` of the provider API |
| destination | `path`, `path_reason` | the copy's path; it is pinned only when a `path_reason` is given, otherwise the copy follows the primary's name |
| destination | `namespace` | GitLab namespace id, Bitbucket project key or GitHub owner login |
| destination | `repository_id` | the copy's immutable id once known (the relay requires it for a GitHub or GitLab member) |
| destination | `mirror_user`, `password_env` | the mirror's login and the environment variable holding its credential |
| destination | `use_ssh`, `interval_seconds`, `sync_on_commit`, `branch_filter` | mirror transport and timing |
| destination | `hold`, `absent`, `remote_name`, `lock_exception` | pause, declarative removal of a mirror, explicit ownership claim, a declared reason that the receiver lock cannot be applied (private GitHub on the free plan) |

There is no field for CI: a placement that carries one is refused (`cfrg/tests/it/zero_ci.rs`).

## Daily run (`cfrg daily`)

`cfrg daily` runs the steps below in order (Renovate is not one of them: run it as its own job), each alone: a failing step is logged and the next still runs, and the
command exits non-zero if any failed. A step whose credential is absent is a no-op that says so. It is started by a
scheduler you provide (cron, a systemd timer, a Kubernetes CronJob); cfrg has no scheduler. Code:
`cfrg-cli/src/daily.rs`. Options: `--credentials-dir` (`CFRG_CREDENTIALS_DIRECTORY`), `--config`
(`CFRG_RELAY_CONFIG`), `--placement` (`CFRG_PLACEMENT`), `--state`, `--ca` (`CFRG_DAILY_CA`),
`--gitlab-key-script`, `--copy-script`, `--gate-dir`, `--max-writes`, `--dry` (`DAILY_DRY_RUN`).

| Step | Runs | Needs in the credentials directory |
|---|---|---|
| `native` | `cfrg native --operation reconcile --apply` (plan when `--dry`) | `forgejo-token`; `cfrg native` reads the provider tokens from the same files (`gitlab-token`; GitHub: `github-token` or an installation token minted from `github-app-id` + `github-app-key` per owner; `bitbucket-token`, without it Bitbucket is skipped with one log line) |
| `settings` | `cfrg native --operation settings --apply`, at most `--max-writes` copies | `forgejo-token` and the provider files above; `<gate-dir>/paused` must exist (the GitHub write gate's pause folder, `<gate-dir>/kill-switch` its kill switch) |
| `wiki` | `cfrg native --operation wiki --apply` | `forgejo-token`, the provider files above, `wiki_deploy_key` (+ `wiki_deploy_key.pub`), `wiki_known_hosts`; the GitLab deploy key id is found by the key's fingerprint at run time (`DAILY_WIKI_KEY_ID` only overrides) |
| `sweep` | `cfrg relay sweep` | the database files `host port dbname username password` |
| `gitlab-key` | the script named by `--gitlab-key-script` (not shipped with cfrg) | `gitlab_token`, `forgejo_push` and the database files |
| `copy` | the script named by `--copy-script` (the very one `hook_commands.copy` runs per push), without a repository: the catch-up for a missed push or new-repository event (`COPY_DRY=1` when `--dry`) | the script file (a no-op when it is not mounted) |

The hooks `/hooks/handoff` (a published release, or a packaging tag `aur/<pkgver>-<pkgrel>` of the owner) and `/hooks/enroll` (a push to `main` that changes the root file `ccvl-downstream.json`) plan one job each: `OWNER/REPO TAG` and `OWNER/REPO AFTER`. Code and tests: `cfrg-serve/src/hooks/handoff.rs`, `enroll.rs`.
