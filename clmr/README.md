# clmr

Resolver over declared stores. Pure selection lives here; the `cfrg resolve`
CLI runs the bounded probes. See the shared `RESOLVER-API.md` contract.

The resolver CLI can read the Worker's version-1 placement document through
`primary_source.placement_file` (an absolute runner-configured path). The file
contains `version`, `default` (a bare HTTPS primary origin), and `primaries`
(lowercase owner/repo exceptions). Origins map through the source's explicit
`identities` table. The file is read once with a 1 MiB limit. Missing, malformed,
unsupported or unmapped placement leaves moving refs on the canonical pointer;
it never silently substitutes an HTTP lookup. Pinned hashes still probe stores
in order independently of primary placement. Explicit policy identities win.

## Decision order (decided 2026-10-07)

Resilience first: no single failure (Cloudflare, the Worker quota, a primary
forge, the internal path) may take a job down. The order per repository:

| Situation | Result |
|---|---|
| The local placement file says the home forge | Declared stores of that identity, in order: the internal express lane first, then the public forge name as a second store. The first store that holds the ref wins. |
| Every such store fails | The canonical pointer (`git.corbet.ch`). |
| The placement file says another forge, or nothing | The canonical pointer, which leads to that primary. |
| `emergency_fallback` is declared, the repository's primary has no declared store, and the pointer or the primary cannot answer | A declared store that holds the moving ref, flagged `emergency-fallback` with a decision note to surface as a warning: the copy may lag the primary. |
| A pinned commit | The first declared store that verifies the exact hash, whatever the primary. Always hash-verified. |

The reachability question is asked only when `emergency_fallback` is on and a
moving ref was not served by a primary-identity store. It is one bounded,
unauthenticated request (no credential reaches the primary's host). A status of
429 (the Workers quota answer), a server error, a timeout or a transport
failure means "cannot answer"; any other status means the path is up.
