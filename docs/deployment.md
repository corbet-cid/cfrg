# Deployment

| Form | State | Notes |
|---|---|---|
| Build from source | available | `cargo build --release -p cfrg-cli`; the binary is `cfrg` (`cfrg-cli/Cargo.toml`, `[[bin]] name = "cfrg"`). Current stable Rust, no exact compiler pin (the workspace sets no `rust-version`). |
| Release binaries | planned (B14) | `cfrg release` already publishes a binary by commit and hash ([release](release.md)); a public release workflow does not exist yet. |
| Container image | planned (B14, with the receiver, B5) | no image definition exists in this repository yet. |
| Nix flake | planned (B14) | no `flake.nix` exists in this repository yet. |

## From source

```sh
git clone https://git.corbet.ch/corbet-cid/cfrg.git
cd cfrg
cargo build --release -p cfrg-cli
./target/release/cfrg source-revision      # the revision embedded at build time (CFRG_SOURCE_REVISION)
```

The workspace has one git dependency, `cqlt`, pinned by revision in `Cargo.toml`.

## Runtime requirements

* `git` (and `curl` 8.3 or newer for `cfrg bridge`, [contribution bridge](pr-bridge.md)).
* Credentials in environment variables only (see [security](security.md)).
* Persistent private storage for `--state` / `--state-dir`: rate windows,
  pacing, uncertain intents. Outside Git.

## Long-running mode

`cfrg serve` is one process per state directory; a second one refuses to start
([serve](serve.md)). It handles SIGTERM and SIGINT by finishing the pass in
progress. Run it under any supervisor; cfrg ships none and has no scheduler
of its own.

## Planned

Container, flake, and an end-to-end quick start on a clean machine are the
subject of B14 and the final documentation pass B17.
