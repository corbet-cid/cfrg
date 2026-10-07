# Observe

`cfrg observe` answers two read-only questions about a repository, each with one
JSON line on standard output. Nothing is written or kept.

```sh
cfrg observe --forge forgejo --origin https://forge.example --token-env FORGE_TOKEN \
  head org/name main
cfrg observe --forge forgejo --origin https://forge.example --token-env FORGE_TOKEN \
  status org/name FULL_COMMIT_ID
```

| Call | Output |
|---|---|
| `head REPOSITORY BRANCH` | `{"repository","branch","head"}`; `head` is the full commit id, or `null` when the branch does not exist |
| `status REPOSITORY COMMIT` | `{"repository","commit","statuses":[{"context","state"}]}` with the latest status per context; `state` is `success`, `failure` or `pending` (`error` counts as `failure`; `warning` and anything unknown as `pending`, never green) |

The token is read from the named environment variable, expanded inside curl and
never appears in an argument or an output line. Requests use the paced transport
with durable rate windows (`--state-dir` or `CFRG_OBSERVE_STATE_DIR`; a temporary
directory when omitted) and stop on the first 401, 403, 429, 402 or server error.
Forgejo implements both natively; the other adapters declare them unsupported
(`cfrg land --capabilities`).
