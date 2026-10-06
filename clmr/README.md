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
