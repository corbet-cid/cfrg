# Quick start

This page takes you from nothing to a running `cfrg-serve` that receives a signed Forgejo event and uploads a release to
a GitLab copy. It needs only Nix with flakes, a Forgejo account and a GitLab account; it creates a throwaway repository
on each and deletes both when it finishes.

## 1. Build

```sh
nix build git+https://git.corbet.ch/corbet-cid/cfrg#cfrg-serve -o cfrg-serve-result
nix build git+https://git.corbet.ch/corbet-cid/cfrg#cfrg -o cfrg-result      # the command line, for the rest of the docs
./cfrg-serve-result/bin/cfrg-serve        # prints the usage and exits 2
```

The flake outputs are `cfrg`, `cfrg-serve` and `cfrg-serve-image` (`flake.nix`).

## 2. What the script does

[`docs/examples/quickstart.sh`](examples/quickstart.sh) is the whole procedure. It needs `cfrg-serve`, `curl`, `git`, `jq`
and `openssl` on the `PATH` and these variables:

| Variable | Meaning |
|---|---|
| `FORGE_URL` | your Forgejo origin, e.g. `https://forge.example.org` |
| `FORGE_TOKEN` | a Forgejo token that may create and delete repositories in its own account |
| `GITLAB_TOKEN` | a GitLab token with scope `api` |
| `GITLAB_PATH` | your GitLab user name or a top-level group you may create projects in |
| `GITLAB_API` | optional, default `https://gitlab.com/api/v4` |
| `PORT` | optional, default `18080` |

```sh
curl -fsSLO https://git.corbet.ch/corbet-cid/cfrg/raw/branch/main/docs/examples/quickstart.sh
export FORGE_URL=https://forge.example.org FORGE_TOKEN=... GITLAB_TOKEN=... GITLAB_PATH=your-gitlab-user
nix shell git+https://git.corbet.ch/corbet-cid/cfrg#cfrg-serve nixpkgs#curl nixpkgs#git nixpkgs#jq nixpkgs#openssl \
  --command bash quickstart.sh
```

The steps, in the order the script runs them:

1. Create a throwaway Forgejo repository `cfrg-qs-throwaway-<timestamp>` and a throwaway GitLab project of the same name
   with its CI switched off (`builds_access_level=disabled`; the upload refuses to write while CI is on).
2. Publish a release `v0.0.1` on Forgejo with one file and a `SHA256SUMS` file (the checksum contract).
3. Push the same commit and tag to the GitLab copy. In production Forgejo's push mirror does this; cfrg
   [configures it](native-mirrors.md).
4. Write the serve configuration and the credentials directory. The configuration is this file (generated with `jq`):

   ```json
   {
     "listeners": {"internal": {"listen": "127.0.0.1", "port": 18080}},
     "forge_api": "https://forge.example.org/api/v1",
     "forge_web": "https://forge.example.org",
     "github": {"api": "https://api.github.com", "uploads": "https://uploads.github.com", "orgs": []},
     "gitlab": {"api": "https://gitlab.com/api/v4", "groups": ["your-gitlab-user"]},
     "aliases": {"gitlab": {"your-forgejo-user/cfrg-qs-throwaway-1": "your-gitlab-user/cfrg-qs-throwaway-1"}}
   }
   ```

   and the credentials directory has one file per key: `hmac_forgejo` (a random shared secret), `forgejo-token`,
   `gitlab-token` ([configuration](configuration.md#credentials-directory)).
5. Start `cfrg-serve serve` with `FORGE_BRIDGE_CONFIG` and `FORGE_BRIDGE_SECRETS` pointing at those.
6. Send the event Forgejo sends for a published release, signed with HMAC-SHA256 over the raw body in
   `X-Forgejo-Signature`. First with a wrong key (answer: 401, nothing happens), then with the right key (answer: 202, the
   upload is queued).
7. Read the result: the log shows the upload, and the script lists the release links on the GitLab project.
8. On exit, whatever happened, it deletes both repositories and prints the HTTP status of a read-back (404 = gone). On
   gitlab.com a deletion is first scheduled and the project renamed; the script then removes it for good.

## 3. Expected output

This is the output of a run from a clean root file system (below), with the repository names shortened:

```text
6. a signed release event (the body Forgejo sends for a published release)
   wrong signature -> HTTP 401 (expected 401)
   right signature -> HTTP 202 (expected 2xx)
7. the result on the GitLab copy
{"event":"listening","github":false,"gitlab":true,"keys":["hmac_forgejo"],"listeners":{"internal":18080},"relay":false,...}
{"event":"refused","listener":"internal","path":"/hooks/release","reason":"wrong",...}
{"event":"accepted","kind":"release","listener":"internal","repo":"<owner>/cfrg-qs-throwaway-1","state":"queued","tag":"v0.0.1",...}
{"copy":"github","detail":"organisation <owner> is not enabled for release upload (config.json github.orgs)","event":"copy","status":"skipped",...}
{"assets":2,"event":"primary-verified",...}
{"asset":"SHA256SUMS","bytes":76,"event":"gitlab-asset-uploaded",...}
{"asset":"hello.txt","bytes":27,"event":"gitlab-asset-uploaded",...}
{"copy":"gitlab","detail":"release and all assets identical, read back","event":"copy","status":"synced",...}
{"event":"done","exit":0,"kind":"release",...}
hello.txt
SHA256SUMS
cleanup: forgejo repository -> HTTP 404, gitlab project -> HTTP 404 (404 = gone)
```

## How this was verified

The script was run end to end on 2026-11-13 inside a root file system that holds nothing but the Nix store, the script
and the network (a `bwrap` sandbox with an empty root: no Rust toolchain, no checkout, no shell profile, no other tools
than the five above, coreutils, grep, sed, awk and bash, and the built `cfrg-serve`), against a throwaway repository on a Forgejo server and a throwaway project
on gitlab.com. The release was uploaded, the wrong signature was refused with 401, and both throwaways were deleted;
the deletion was read back as 404 and a listing of the account's projects showed none left. The build used was the flake
at the revision this page was written against; `nix build` of the public repository gives the same package.

## Where next

* Real deployments run `cfrg native` to create the push mirrors and the copy settings
  ([native mirrors](native-mirrors.md)), `cfrg-serve` behind a TLS terminator for the public listener
  ([configuration](configuration.md), [deployment](deployment.md)), the relay for issues and pull requests
  ([relay](relay.md)) and `cfrg daily` for the periodic reconcile ([configuration](configuration.md#daily-run-cfrg-daily)).
* Webhook registration on the forge is yours: a Forgejo system or organisation webhook of type `release` pointing at
  `/hooks/release` with the secret in `hmac_forgejo`; GitHub and GitLab webhooks point at `/hooks/github` and
  `/hooks/gitlab` of the public listener.
