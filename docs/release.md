# Release

`cfrg release` publishes a build artifact for one exact commit on the primary
forge, so consumers fetch it by URL and hash (Nix `fetchurl`) instead of a
manual install on a host.

```sh
cfrg release --config .ci/release.json publish --self        # CI: publish this very binary
cfrg release --config .ci/release.json publish --file out/tool --commit "$SHA"
cfrg release --config .ci/release.json publish --file out/tool --commit "$SHA" --dry-run
cfrg release --config .ci/release.json show --commit "$SHA" --name tool-x86_64-unknown-linux-gnu
```

`--self` publishes the running executable under the commit it was built from
(`cfrg source-revision`) as `<package>-<target triple>`: the bytes and the
commit cannot disagree. `--dry-run` prints the same record (`url`, `sha256`,
`size`) without uploading or reading a token, so a release job can be
rehearsed before the first real publication.

## Build environment

A release binary must be built by the CI environment: the repository job's
check-shell or the job image. On Linux, `publish` (both `--self` and `--file`)
reads the ELF `PT_INTERP` and refuses a binary whose program interpreter is not
under `/nix/store/`. The CI image and every Nix check-shell build with a Nix
loader; a binary built on a plain host links that host's loader
(`/lib64/ld-linux-*.so.*`) and may require a newer glibc than the image runs,
which fails every consumer. A static binary (no `PT_INTERP`) is accepted; a
non-ELF file is not judged. The refusal happens before any upload or token
read, so a mistaken local build costs nothing.

## Forgejo: generic package registry

Releases attach files to tags that can be re-pointed and replaced; the generic
package registry is immutable per version, which is what a content hash needs.

| Property | Verified on Forgejo 15.0.9 |
|---|---|
| URL | `https://forge.corbet.ch/api/packages/<owner>/generic/<package>/<commit>/<file>` |
| Immutable | first `PUT` 201, any later `PUT` 409, also with different bytes |
| Anonymous download | yes for public owners, no token needed |
| Hash | the files listing carries the forge-computed SHA-256; `publish` compares it with the local file |
| Idempotent | republishing identical bytes reports the same URL and `created: false`; different bytes are an error |

The command prints `{url, sha256, size, commit, package, file, created}`. A Nix
consumer pins that URL and hash:

```nix
pkgs.fetchurl { url = "<url>"; hash = "sha256-..."; executable = true; }
```

## Config (JSON, declared next to the CI manifest)

```json
{"schema": 1, "forge": "forgejo",
 "endpoint": {"origin": "https://forge.corbet.ch", "token_env": "CFRG_RELEASE_TOKEN"},
 "owner": "corbet-cid", "package": "cfrg"}
```

The token needs package write for the owner. Other forges answer `unsupported`
(`cfrg land --capabilities` prints the declared support).

## Automatic publication

* The repository declares a `release` job in `.ci/ccid.toml` whose check runs
  `cfrg release publish --self` on the freshly built binary (cfrg itself does
  this; see its manifest).
* After a commit lands, the landing policy's `landed` command starts that job
  for the exact landed commit (`docs/land.md`).
* The job needs `CFRG_RELEASE_TOKEN` in the trusted scheduler environment: a
  token that can only write packages for the owner. It must never reach a
  contributed workflow file.
* Consumers that run `cfrg` or `ccid` fetch the printed URL by hash instead of
  a manual install under `/workspaces/ci-tools`.

