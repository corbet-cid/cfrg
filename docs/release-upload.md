# Release upload

`cfrg-serve` copies a published Forgejo release to the GitHub and GitLab copies: the files and the release notes. Code:
`cfrg-serve/src/release/` (`mod.rs` header, `github.rs`, `gitlab.rs`, `primary.rs`); tests: `cfrg-serve/tests/release.rs`.

Trigger: a signed `release` event on `POST /hooks/release`, or by hand
`cfrg-serve release-upload <github|gitlab|all> OWNER/REPO TAG [--plan]` (and `cfrg-serve sync OWNER/REPO TAG [--plan]`
for both copies). `--plan` writes nothing.

The request is a hint: the module is told `OWNER/REPO TAG` and reads everything else from the Forgejo primary. Per copy:

1. Read the release of the tag on the primary. Its checksum contract must cover every asset: one `SHA256SUMS`-style file
   (`SHA256SUMS`, `SHA256SUMS.txt`, `sha256sums.txt`, `checksums.txt`, `checksums.sha256`, `CHECKSUMS`) or one
   `<asset>.sha256` per asset. A release that fails its own contract is not uploaded anywhere.
2. Compare. Nothing to write is `in_sync` with zero writes.
3. Gates before the first write: the tag is on the copy at the same commit (this module never creates a tag and never
   pushes git, the mirrors do); CI is provably off on the copy; the repository is not a contributed project.
4. Write: every asset is verified against the contract first, then uploaded one at a time. An asset that already
   exists with other content is never replaced. GitHub gets release assets; GitLab gets generic package files linked from
   the release.
5. Read the copy back.

Statuses: `in_sync`, `synced`, `no_copy`, `skipped`, `refused` (done); `pending_tag`, `blocked_ci`, `rate_limited` (exit
75: incomplete, not a fault); `failed` (exit 1). Bitbucket is reported `skipped` (git and tags only). There is no retry
loop of its own: the next event, the start of the service or the daily run completes a stopped copy (see Release intents). A repository is uploaded only when its GitHub organisation or GitLab group
is listed in the configuration (`github.orgs`, `gitlab.groups`; [configuration](configuration.md)).

## GitHub credential

The GitHub copy takes a plain `github-token` if one is mounted, otherwise the installation token of the GitHub App
(`github-app-id`, `github-app-key`) for the organisation that owns the copy: the App is installed per organisation, and
the installation is resolved with the App JWT and cached in memory (`cghb`). Nothing is configured per organisation.

## Release intents

A restart (any Secret change restarts the pod) must not lose a release in flight, and no second release event arrives to
retry it. The route therefore writes an intent (table `release_intents` in the bridge database, `repo`, `tag`, `since`)
BEFORE it answers 202; when the intent cannot be written it answers 503 and the forge redelivers. The intent is cleared
when the sync ends with exit 0. Open intents are finished on service start and by the daily run
(`cfrg-serve release-resume`, the `releases` step of `cfrg daily`; exit 1 while one stays open). The upload itself is
idempotent ("identical, read back"), so a re-run is safe. There is no queue and no timer beyond that.

The push of this repository to its git copy is a serve event too (`/hooks/copy`, workloads `forge-bridge/hooks`): see [deployment](deployment.md).
