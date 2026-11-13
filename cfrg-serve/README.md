# cfrg-serve

The receiver and the release upload of the cfrg bridge (Bridge design, task B5). One small process, two listeners.
Events only: no timer, no polling, no queue of its own beyond a bounded in-memory hand-off (R30).

| Listener | Route | Key | Does |
|---|---|---|---|
| internal (TLS 1.3, cluster CA) | `POST /hooks/release` | `hmac_forgejo` | Forgejo `release` hook -> release upload to the GitHub and GitLab copies (R45) |
| internal | `POST /hooks/sync` | `hmac_forgejo` | a tag pusher's hint `{"repository","tag"}` -> the same upload |
| internal | `POST /hooks/forgejo` | `hmac_forgejo` | issues, comments, pull requests of the primary -> `relay.yml` |
| public (plain HTTP behind the tunnel) | `POST /hooks/github` | `hmac_github` | `X-Hub-Signature-256` -> `relay.yml` |
| public | `POST /hooks/gitlab` | `hmac_gitlab` | Standard Webhooks (`webhook-signature`, key = the decoded `whsec_` token) -> `relay.yml` |
| both | `GET /healthz` | none | liveness |

A listener answers nothing outside its rows. Every signature is checked on the raw body before it is parsed
(constant time). A delivery id (`X-GitHub-Delivery`, `webhook-id`, `X-Forgejo-Delivery`) is remembered for a day:
a replay answers `200 duplicate`. The receiver answers `202` as soon as the job is queued; the dispatch itself is one
attempt (no retry), and a failed one releases the id so the sender's redelivery is accepted.

## The dispatch

`POST {forge_api}/repos/{dispatch.repository}/actions/workflows/relay.yml/dispatches` with
`{"ref": "main", "inputs": {forge, repository, kind, number, action}}`, all strings. `repository` is the forge's
immutable repository id, `kind` is `issue` or `contribution`, `action` is `changed`, `deleted` or
`comment_deleted:<comment id>`. `event::Dispatch::hint` turns the inputs into the relay's own `Hint`, so the workflow
and the receiver cannot drift. Nothing else of the payload is forwarded. The credential is `dispatch_token`, a Forgejo
token limited to the one repository (`write:repository`).

## The release upload

Ported from the Python bridge; its tests are the parity contract (`tests/release.rs`). Verified against
`SHA256SUMS` (or `<asset>.sha256`), never creates a tag, never pushes git, writes only where CI is provably off, never
replaces an asset with other content, refuses a contributed project (`Contributed::refuse`, R10/R11) on the primary and
on each copy, and answers `pending_tag` (exit 75) until the tag is on the copy.

## Run

```
cfrg-serve serve                          # config: FORGE_BRIDGE_CONFIG, credentials: FORGE_BRIDGE_SECRETS (a directory)
cfrg-serve sync OWNER/REPO TAG [--plan]   # one release sync by hand; exit 0, 75 or 1
```

Credentials are files named `forge_token`, `github_token`, `gitlab_token`, `dispatch_token`, `hmac_forgejo`,
`hmac_github`, `hmac_gitlab`.
