# Configuration reference

cfrg reads JSON or TOML files named on the command line. Secrets are never in a
file: a file names an environment variable (`token_env`, `password_env`,
`secret_env`) and cfrg reads the value at the last moment
(`cfrg/src/native.rs` validates the variable name with `env_name`).

| File | Command | Full description |
|---|---|---|
| placement (`--placement`) | `cfrg native` | [native mirrors](native-mirrors.md) |
| state (`--state`) | `cfrg native` | persistent private storage with rate windows, pacing, uncertain intents and ownership IDs; never credentials ([native mirrors](native-mirrors.md)) |
| repository policy (`--policy`) | `cfrg validate`, `plan`, `decide`, `clone`, `sync` | [repository policy](repository-policy.md); example `cfrg/examples/repository-policy.json` |
| landing policy (`--policy`, with an optional `serve` block) | `cfrg land`, `cfrg serve` | [land](land.md), [serve](serve.md) |
| gate configuration (`--config`) | `cfrg gate` | [gate](gate.md) |
| release configuration (`--config`) | `cfrg release` | [release](release.md) |
| bridge configuration (`--config`, TOML) | `cfrg bridge` | [contribution bridge](pr-bridge.md) |
| identities (`--identities`, TOML) | `cfrg access` | [access sync](access-sync.md); example `docs/examples/identities.toml` |
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

## Planned configuration

The relay, the GitHub and GitLab receiver and the daily run will need their own
configuration; their formats are not fixed yet (B2, B5, B6). The final
documentation pass fills this page from the built code (B17).
