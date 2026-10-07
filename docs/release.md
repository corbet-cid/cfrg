# Release

`cfrg release` publishes a build artifact for one exact commit on the primary
forge, so consumers fetch it by URL and hash (Nix `fetchurl`) instead of a
manual install on a host.

```sh
cfrg release --config .ci/release.json publish --self        # CI: publish this very binary
cfrg release --config .ci/release.json publish --file out/tool --commit "$SHA"
cfrg release --config .ci/release.json show --commit "$SHA" --name tool-x86_64-unknown-linux-gnu
```

`--self` publishes the running executable under the commit it was built from
(`cfrg source-revision`) as `<package>-<target triple>`: the bytes and the
commit cannot disagree.

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
 "owner": "corbet-libs", "package": "cfrg"}
```

The token needs package write for the owner. Other forges answer `unsupported`
(`cfrg land --capabilities` prints the declared support).
