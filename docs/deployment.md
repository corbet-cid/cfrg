# Deployment

| Form | Where it is defined | Notes |
|---|---|---|
| Nix flake | `flake.nix` | outputs `cfrg`, `cfrg-serve` and `cfrg-serve-image` for `x86_64-linux` and `aarch64-linux`, and a NixOS module `nixosModules.default` with `programs.cfrg.enable`, which only puts `cfrg` on the system path. Services are the deployer's own configuration. |
| Container image | flake output `cfrg-serve-image` | a layered image (`dockerTools`) with the `cfrg-serve` binary and a CA bundle, nothing else, running as an unprivileged user (`65534:65534`). `nix build .#cfrg-serve-image` produces the archive for `docker load` or `podman load`. |
| Build from source | `cfrg-cli/Cargo.toml`, `cfrg-serve/Cargo.toml` | `cargo build --release -p cfrg-cli -p cfrg-serve`; the binaries are `cfrg` and `cfrg-serve`. Current stable Rust, no exact compiler pin (the workspace sets no `rust-version`). |

cfrg publishes no prebuilt binaries yet. Build from the flake or from source.

## With Nix

```sh
nix build git+https://git.corbet.ch/corbet-cid/cfrg#cfrg-serve
nix build git+https://git.corbet.ch/corbet-cid/cfrg#cfrg
nix build git+https://git.corbet.ch/corbet-cid/cfrg#cfrg-serve-image && docker load < result
```

The [quick start](quickstart.md) runs the first of these end to end.

## From source

```sh
git clone https://git.corbet.ch/corbet-cid/cfrg.git
cd cfrg
cargo build --release -p cfrg-cli -p cfrg-serve
./target/release/cfrg source-revision      # the revision embedded at build time (CFRG_SOURCE_REVISION)
```

The four drivers (`cfgj`, `cghb`, `cglb`, `cbkt`) and `cqlt` are git dependencies, each pinned by revision in `Cargo.toml`.
A Nix build of those needs their hashes in `flake.nix` (`outputHashes`); the CI check `verify` evaluates the flake
packages so that a missing hash fails the push.

## Runtime requirements

* `git` (and `curl` 8.3 or newer for `cfrg bridge`, [contribution bridge](pr-bridge.md)).
* `cfrg native`: environment variables named in the placement (`token_env`, `password_env`) hold the tokens.
* `cfrg-serve`: a configuration file and a credentials directory ([configuration](configuration.md)), a TLS-terminated way
  to reach the `public` listener from GitHub and GitLab (the listener itself can serve TLS 1.3 from `tls_dir` or plain
  HTTP behind a terminator), and, for the relay, a PostgreSQL database whose connection is verified against a CA file.
* Persistent private storage for `--state` / `--state-dir`: rate windows, pacing, uncertain intents. Outside Git.

## Long-running modes

* `cfrg-serve serve` is the receiver: signed webhooks, in-process relay, release upload, wiki push. It stops on SIGTERM or
  SIGINT. Run it under any supervisor; the example in the quick start runs it as a background process.
* `cfrg serve` is the older land lane: signed webhooks and a periodic sweep drive landing and mirror verification. It is one
  process per state directory; a second one refuses to start ([serve](serve.md)).
* `cfrg daily` is one run for a scheduler to start ([configuration](configuration.md#daily-run-cfrg-daily)). cfrg ships no scheduler.
