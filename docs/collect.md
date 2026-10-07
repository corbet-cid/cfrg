# Forge evidence collection

`cfrg collect` collects read-only forge facts and writes a saved snapshot for
offline evaluation through the separate
[cqlt](https://git.corbet.ch/corbet-cid/cqlt) Rust library (see
`ccid quality check`). Network collection is separate from deterministic
evaluation; saved evidence can be reviewed and checked offline.

```sh
cfrg collect --forge github --output github.json
cfrg collect --forge github --org example --output example.json
```

GitHub collection uses the existing `gh` login, or a `CFRG_TOKEN` environment
variable. Other instances require `CFRG_TOKEN` and an explicit HTTPS API root:

```sh
# Supply CFRG_TOKEN through your existing secret manager, never a command argument.
cfrg collect --forge forgejo --api-url https://forge.example/api/v1 \
  --output forgejo.json
cfrg collect --forge github --api-url https://github.example/api/v3 \
  --org example --output enterprise.json
```

Collection requires an existing curl 8.3+ (environment variable expansion) and,
when using its login, `gh`. Credentials stay in process memory/environment;
curl expands them internally, keeping them out of command arguments and files.
No redirect is followed. API responses and snapshots may contain private
metadata: keep them in private storage and CI. Snapshot files are written
atomically with private permissions. Every request has a 30-second timeout and
16-MiB response limit; `--timeout` bounds the overall collection (default 900s).
No tool is installed, repository cloned, content executed or forge data modified.

With no `--org`, collection paginates membership organizations. Explicit
organizations are also supported. This is the credential's accessible scope;
it cannot prove the absence of repositories hidden by permissions. Personal
account repositories are outside the organization scan. Pagination continues
to an empty page even when a forge caps page size; repeated identities fail
closed. Failed organization reads remain visible in the requested scope.
Failed content reads become unknown, not missing or passing.

Both adapters resolve the default branch and inspect contents at that commit.
GitHub's native README API handles supported README locations. Forgejo checks
the root README. Root license filenames accept LICENSE, LICENCE or COPYING,
including dot, hyphen and underscore suffixes; license suitability is not
inferred. Organization profiles use `.github/profile/README.md` on GitHub and
`.profile/README.md` on Forgejo. Accessible private profiles are recorded as
documentation, not proof of a public profile. The first rule set checks presence
and nonzero byte size, not prose quality or public rendering.

Collection exit 0 means collection completed, not that quality passed. Evaluate
the snapshot separately with cqlt. A committed snapshot is historical evidence;
it does not establish current forge health. Scheduling and audit storage belong
to the caller, not this command.
