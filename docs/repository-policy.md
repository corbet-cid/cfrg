# Repository placement and failover

`cfrg validate --policy forges.json` validates a declaration without API
writes. `cfrg plan --policy forges.json --repository widget` reports the
primary and ordered read fallbacks. See [the complete example](../examples/repository-policy.json).

The primary CI names its forge; every repository must have a location there.
Locations are explicit and use full instance/namespace identity, including nested
GitLab groups. GitHub, Forgejo, GitLab, Bitbucket and other Git forges share the
same read transport. Attribute rules constrain declarations; they do not infer
ownership or create repositories. Visibility uses cqlt's type. Declared attributes
remain claims until a collector verifies them; presentation checks remain in cqlt.

`cfrg sync` provides explicit, bounded, fast-forward-only reconciliation from
the declared primary to selected secondaries. See [its safety and completeness
contract](forge-sync.md), including offline replicas and external content.

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
`absent` provider with the requested coverage can receive new work. Execution
fallback never changes the primary forge or creates a second trigger.

The existing Crow/GitHub dispatcher retains its exact-request receipts, inventory,
locking and cancellation checks. These generic policy functions are reusable by
additional adapters; they are not an autonomous failover service. Paid providers
are refused by `free_only`; public free hosted execution also requires public,
nonsensitive source. Owned execution remains available for private repositories.
