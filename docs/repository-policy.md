# Repository placement and failover

`cfrg validate --policy forges.json` validates a declaration without API
writes. `cfrg plan --policy forges.json --repository widget` reports the
primary and ordered read fallbacks. See [the complete example](../cfrg/examples/repository-policy.json).

The primary CI names its forge; every repository must have a location there.
Locations are explicit and use full instance/namespace identity, including nested
GitLab groups. GitHub, Forgejo, GitLab, Bitbucket and other Git forges share the
same read transport. Attribute rules constrain declarations; they do not infer
ownership or create repositories. Visibility uses cqlt's type. Declared attributes
remain claims until a collector verifies them; presentation checks remain in cqlt.

`cfrg sync` provides explicit, bounded, fast-forward-only reconciliation from
the declared primary to selected secondaries. See [its safety and completeness
contract](forge-sync.md), including offline replicas and external content.
A repository that contains Git LFS pointers or submodule gitlinks is refused by
default. An exact per-location exception may be declared beside its locations:

```json
"locations": {"forgejo": "team/widget", "gitlab": "team/widget"},
"external_content": {"gitlab": "pointers-only"}
```

This permits refs and pointer blobs to reach that secondary while the LFS
payloads and submodule commits stay at the primary; the sync report states
`lfs: pointers-only` and/or `submodules: pointers-only` and never reports the
repository complete.

`cfrg clone --policy forges.json --repository widget --commit FULL_SHA
--destination NEW_DIRECTORY` tries the primary then the declared clone fallbacks.
Each attempt has a bounded timeout and an isolated object database. Only the
requested commit may be checked out. Existing destinations are refused. Authentication
uses Git's existing credential setup, never credentials in policy URLs. This checks
out tracked Git content only: submodule objects and LFS payloads still require exact
source staging. It never pushes, rewrites a mirror, changes visibility or promotes
a writable primary. HTTPS clone endpoints are required; other Git transports can
be selected through the operator's Git configuration.

`cfrg decide --policy forges.json --repository widget --require linux-x86_64
--observations states.json` is a pure decision for a provider adapter. Observations
must describe **one identical request**: source, dependencies, tool, configuration,
runtime and coverage. A `succeeded` observation requires the corresponding receipt;
the library does not collect that proof itself. The adapter must serialize inventory
and dispatch and retain intent across ambiguous responses. Missing or `unknown`
observations block dispatch. `active` attaches, `failed` remains failed, and verified
`succeeded` reuses. `unavailable` means no unresolved submission exists; an outage
that prevents checking an earlier submission is `unknown`. `cancelled-before-start`
requires confirmed terminal cancellation and proof no execution began. Only an
`absent` primary with the requested coverage can receive new work. The primary is
the repository's only executor: there is no fallback to turn to, so an unavailable,
cancelled or unknown primary blocks.

**No CI executor on a mirror forge can be modelled** (no CI runs
on a mirror). Every executor is `"execution": "owned"` (our own compute); a
provider-hosted (`free-hosted`) or `paid` executor does not parse; the own CI of a
mirror forge (`github-actions`, `gitlab-ci`, `bitbucket-pipelines`) is refused as a
driver; and a repository's `execution_fallbacks` (the one way another forge's executor
could be named) must be empty or absent: any executor in it is refused. The empty key and
`free_only` are still accepted, because deployed documents carry them; neither has an effect.

The existing Crow dispatcher retains its exact-request receipts, inventory,
locking and cancellation checks. These generic policy functions are reusable by
additional adapters; they are not an autonomous failover service.
