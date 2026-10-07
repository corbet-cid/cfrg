# Repository contents

`cfrg contents` reads named files of repository branches through the forge API,
never by clone. A consumer that has to inspect many repositories (for
example which of them pin a landed library) asks once for the whole set and
keeps the answers by blob id.

```sh
cfrg contents --forge forgejo --origin https://forge.example \
  --token-env FORGE_TOKEN [--state-dir STATE] < query.json
```

The query is one JSON document on standard input:

```json
{
  "targets": [{"repository": "org/name", "branch": "main"}],
  "paths": ["Cargo.lock", "Cargo.toml", "flake.lock", "flake.nix", ".ci/ccid.toml"],
  "known": ["<blob id the caller already holds>"]
}
```

Standard output carries one JSON line per target, in request order:

| `state` | Meaning | Other fields |
|---|---|---|
| `found` | the branch exists | `files`: for each wanted path that exists, in request order, `path`, `blob` (the content address) and `content` (base64 of the bytes, absent when the blob is listed in `known`) |
| `absent` | the repository, branch or tree does not exist, or the repository is empty | none |
| `failed` | not read | `error`, with no credential and no response body |

Exit code 0 means every target was read (found or absent), 2 means some were not
(or the query was refused; nothing is printed then).

Rules:

* A warm read costs one tree request per target for files at the root, plus one
  per directory a wanted path passes through (read once per target); bytes are
  fetched only for blob ids not in `known`. Wanted paths are at most sixteen,
  at most four `/`-separated names deep, each name `[A-Za-z0-9._+-]`; a
  listing is read page by page, at most five pages. A path that is a directory,
  or passes through a file, does not exist.
* The token is read from the named environment variable, expanded inside curl and
  never appears in an argument, a line of output or a state file.
* Requests are spaced by `--gap-ms` (default 100) so a scan of hundreds of
  repositories stays under a forge's burst limit.
* Reads use the paced transport with durable rate windows (`--state-dir` or
  `CFRG_CONTENTS_STATE_DIR`; a temporary directory when omitted). The first
  transport failure (a recorded rate window, a refused credential, a server
  error, an oversized reply) stops the run: later targets are reported as not read
  instead of being asked again, and the rerun is cheap because the caller's
  known blobs skip every fetch.
* Forgejo implements this natively (git tree and blob API). GitLab, Bitbucket and
  GitHub declare it unsupported; `cfrg land --capabilities` prints the matrix.
