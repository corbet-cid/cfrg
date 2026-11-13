# cfrg

cfrg keeps a project that lives on a Forgejo (or Codeberg) server present on
GitHub, GitLab and Bitbucket, without running anything on those three.

Status: pre-1.0 (`0.1.0`). Parts are built and tested; parts are planned. Every
feature below carries either the code or test that proves it, or the task that
will build it (`planned`, B-numbers from the project's build plan). Nothing
marked planned exists yet.

## The problem

Open-source projects want a home they control, and most of their users and
contributors are on GitHub and GitLab. Maintaining the project in several places
by hand means copied settings that drift, release files uploaded twice, issues
split across forges, and CI minutes spent on every copy. Hosted copies also
bring their own automation (Actions, pipelines, Dependabot) that starts running
the moment code arrives.

cfrg treats one forge as the platform and the others as copies. Code, settings
and releases flow outward from the primary; copies never run CI.

## Who it is for

Projects whose primary is Forgejo (including Codeberg) and that also want to be
found, starred, forked and reported on at GitHub, GitLab and, for code only,
Bitbucket. You need a Forgejo account that can set up push mirrors, and a token
for each copy.

It is not for you if your primary is GitHub or GitLab. A switch of primary is
documented ([switch](docs/switch.md)) but cfrg is designed around Forgejo.

## What it does and does not do

| Capability | State | Evidence |
|---|---|---|
| Push mirrors from Forgejo to GitLab and Bitbucket, created and reconciled per repository | built | [native mirrors](docs/native-mirrors.md); `cfgj/src/native.rs`, `cglb/src/native.rs`, `cbkt/src/native.rs` |
| Copy settings (visibility, description, topics, default branch, links back to the primary) | built | `cfrg/src/native/links.rs` unit tests |
| Receiver lock: only the mirror may write the default branch of a copy | built | [native mirrors](docs/native-mirrors.md#the-receiver-lock-protects-the-default-branch-only) |
| CI and Dependabot switched off on every copy, with no way to switch them on | built | `cghb/src/native.rs` test `actions_and_dependabot_are_switched_off_with_no_way_to_keep_them_on`; `cfrg/tests/it/zero_ci.rs` |
| GitHub copies fed through one paced writer with budgets, kill switch and ledger | built | [gate](docs/gate.md); tests in `cfrg-cli/tests/it/gate_*.rs` |
| Merge only exact green commits into the primary | built | [land](docs/land.md) |
| Webhook-driven long-running mode for landing and mirror verification | built | [serve](docs/serve.md); `cfrg/src/serve.rs` tests |
| Release publication by commit and hash | built | [release](docs/release.md) |
| Opt-in import of GitLab merge requests as WIP pull requests on the primary | built, experimental | [contribution bridge](docs/pr-bridge.md); `cfrg/src/bridge/tests.rs`, `cfrg-cli/tests/it/pr_bridge_cli.rs` |
| Projects that are not ours are never mirrored or relayed | built | [contributed](docs/contributed.md); `cfrg/src/contributed.rs` |
| Issues, comments and pull requests relayed between forges | planned (B2, B3, B8) | none yet |
| Signed receiver for GitHub and GitLab webhooks that starts a Forgejo workflow | planned (B5, B7) | none yet |
| Release file upload to GitHub and GitLab copies | planned (B5) | none yet |
| Wiki push to GitLab copies | planned (B10) | none yet |
| `cfrg take-in` of a reviewed outside contribution | planned (B13) | none yet |
| Release binaries, container image, Nix flake | planned (B14) | none yet |

What it does not do: it does not run CI or any check executor, it does not
schedule itself (schedulers dispatch; [serve](docs/serve.md) reacts to
webhooks), it does not create GitLab or Bitbucket accounts, namespaces or
workspaces, and it never deletes a destination repository
([native mirrors](docs/native-mirrors.md)). It does not adopt the F3 format.

## The flow

```
                 contributors, issues, stars
        GitHub ◀──────────┐         ┌──────────▶ GitLab
          ▲               │         │               ▲
          │ gate          │         │ native push   │
          │ (paced push)  │         │ mirror        │
          │               │         │               │
          └───────────  Forgejo (primary)  ─────────┘
                           │   ▲
                           │   │ merge when green (cfrg land)
                 native    │   │
                 push      ▼   │
                 mirror   Bitbucket (code only)

   Copies: no CI, no Dependabot, default branch locked to the mirror.
   Outside contributions: planned relay (B2, B8) and take-in after review (B13).
```

## Quick start

cfrg is built from source today (binaries, a container and a Nix flake are
planned, B14; see [deployment](docs/deployment.md)).

```sh
cargo build --release -p cfrg-cli        # produces target/release/cfrg
export FORGE_TOKEN=...                   # Forgejo token, read from the environment only
export GITLAB_TOKEN=...
cfrg native --placement placement.json --state native-state.json                      # plan (the default operation): writes nothing
cfrg native --placement placement.json --state native-state.json --operation status   # read-only status
cfrg native --placement placement.json --state native-state.json --operation reconcile --apply
```

`placement.json` is the one configuration file; a minimal example is in the
[configuration reference](docs/configuration.md). The default operation is `plan`
(`cfrg-cli/src/native.rs`, `default_value = "plan"`); changes are made only
with `--operation reconcile --apply`.

## Documentation

| Page | Contents |
|---|---|
| [Why we built it](docs/why-we-built-it.md) | the short story, fit for an announcement |
| [Concepts](docs/concepts.md) | primary, copy, placement, drivers, contributed projects |
| [Configuration](docs/configuration.md) | every config file and where its fields are defined |
| [Deployment](docs/deployment.md) | binary, container, Nix flake |
| [Security model](docs/security.md) | zero CI on copies, credentials, take-in after review |
| Per-feature pages | [native mirrors](docs/native-mirrors.md), [gate](docs/gate.md), [land](docs/land.md), [serve](docs/serve.md), [release](docs/release.md), [switch](docs/switch.md), [access sync](docs/access-sync.md), [collect](docs/collect.md), [contents](docs/contents.md), [observe](docs/observe.md), [native status](docs/native-status.md), [repository policy](docs/repository-policy.md), [contribution bridge](docs/pr-bridge.md), [contributed](docs/contributed.md) |

The library forbids unsafe code (`Cargo.toml`, `unsafe_code = "forbid"`).

## Contributing and contact

See [CONTRIBUTING.md](CONTRIBUTING.md). Contact goes through the issue tracker
of the project; this repository publishes no personal contact addresses.
Changes are recorded in [CHANGELOG.md](CHANGELOG.md).

## License

LGPL-3.0-only WITH LGPL-3.0-linking-exception (SPDX expression). The texts are
in `LICENSES/`; per-file licensing is declared in `REUSE.toml`. The linking
exception is a permission granted on top of the LGPL for conveying combined
works; its exact terms are in `LICENSES/LGPL-3.0-linking-exception.txt`.
