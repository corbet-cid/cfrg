# cfrg (core)

Forge-independent model (`model`: forges, access levels, bridge descriptors),
pure planners (`placement` failover decide, `access` three-way merge,
`profile` projection, `sync` ref table) with no network/clock/filesystem
inside, bounded transports (`process`, Git, curl) and the paced
`access::apply_plan` executor: stops on the first 401/403/429/402 and records
state.

Capability traits live with their planners — `access::{RoleMap, Grants}`,
`collect::{Transport, EvidenceSource}`, `status::{Transport, StatusTarget}`,
`profile::ProfileConvention` — and are implemented once per adapter (`cfgj`,
`cglb`, `cbkt`, `cghb`). Nothing here names a forge's API quirks; the CLI
supplies the implementations.
