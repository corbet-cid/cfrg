# Serve

`cfrg serve` is the long-running mode: neither agents nor timers have to poll.
Signed webhook deliveries bring a repository's pass forward; a slow periodic
sweep makes the loop correct even when the forge drops a delivery (Forgejo has
no `status` event and no delivery retry, so CI results are noticed by the
passes themselves).

| Part | What it does |
|---|---|
| receiver | `POST /hook`: verifies the HMAC-SHA256 signature of the raw body (`X-Forgejo-Signature`, also `X-Gitea-Signature` and `X-Hub-Signature-256`) before parsing anything, then marks the repository due. `GET /healthz` (liveness), `GET /status` (lanes, last results, accepted and rejected counts) |
| land lane | the pass `cfrg land --step` runs: merge green heads, rebase and retest when the base moved, notice merges the forge made alone, start the `retest` and `landed` hooks. Waiting work is looked at every `interval_seconds`, idle repositories on the sweep or the next delivery |
| mirror lane | read-only verification of the declared push mirrors (`cfrg native --operation status`): a push to the default branch verifies that repository after `delay_seconds`, every repository at least every `sweep_seconds` |
| registration | `cfrg serve --register [--apply] [--rotate]` reconciles the webhooks on the forge |

With `"follow": "serve"` in the policy, `cfrg land REPO BRANCH` only enqueues
the pull request and schedules the native merge, then returns; the
`pull_request` delivery makes serve do the rest. Without it the detached
follower of `docs/land.md` keeps working, so both can coexist during a rollout.

## Policy (`serve` block of the landing policy)

```json
{"follow": "serve",
 "serve": {
   "listen": "0.0.0.0:8080",
   "secret_env": "CFRG_WEBHOOK_SECRET",
   "sweep_seconds": 3600, "debounce_seconds": 3,
   "webhook": {"url": "http://cfrg-serve.ci.svc.cluster.local:8080/hook", "orgs": ["corbet-libs"]},
   "mirrors": {"placement": "/etc/cfrg/placement.json", "state": "/var/lib/cfrg/mirror-state.json",
               "sweep_seconds": 21600, "delay_seconds": 60}}}
```

* The secret (at least 16 characters) and the forge token come from the
  environment (`secret_env`, the policy's `endpoint.token_env`), never from files.
* `webhook.orgs` get one organisation hook each; declared repositories of other
  owners get a repository hook. Registered events: `push`, `pull_request`,
  `pull_request_sync` (Forgejo stores `pull_request` expanded into all its
  sub-events, which serve accepts and coalesces within `debounce_seconds`).
* The mirror lane needs the source token named in the placement file.

## Security

* Unsigned or wrongly signed requests get 401 and nothing is parsed; the
  comparison is constant time. A captured valid delivery can be replayed, which
  only causes one more idempotent pass.
* Only declared repositories are acted on; events about others are ignored.
* Bounded input: 16 KiB of headers, 4 MiB body, 64 headers, `Content-Length`
  only (no chunked bodies), 10 s per read and 30 s per request, 32 concurrent
  connections, one request per connection.
* Std only, safe Rust, no new dependencies. HMAC-SHA256 is checked against the
  RFC 4231 vectors.

## Operation

* One server per state directory (it owns the request state, the journal, the
  hook output and the work clones); a second one refuses to start. A lock file
  left by a killed predecessor is cleared at start.
* SIGTERM or SIGINT stops the accept loop and the workers; a pass in progress
  finishes first (hooks are bounded to ten minutes).
* First passes run a few seconds after start, so a restart catches up on
  whatever happened meanwhile.
* Hook commands run inside the process, so the image must contain what the
  policy's `retest` and `landed` commands call.
