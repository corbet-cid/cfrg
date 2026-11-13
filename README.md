# cfrg

cfrg keeps a project that lives on a Forgejo (or Codeberg) server present on
GitHub, GitLab and Bitbucket, without running anything on those three.

Status: pre-1.0 (`0.2.0`, see the [changelog](CHANGELOG.md)). Every feature below names the code, test or page that
backs it. What is not in the table does not exist.

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
| Push mirrors from Forgejo to GitLab and Bitbucket, created and reconciled per repository | built | [native mirrors](docs/native-mirrors.md); `src/native.rs` of the [cfgj](https://git.corbet.ch/corbet-libre/cfgj) repository, `src/native.rs` of the [cglb](https://git.corbet.ch/corbet-libre/cglb) repository, `src/native.rs` of the [cbkt](https://git.corbet.ch/corbet-libre/cbkt) repository |
| Copy settings (visibility, description, topics, default branch, links back to the primary) | built | `cfrg/src/native/links.rs` unit tests |
| Receiver lock: only the mirror may write the default branch of a copy | built | [native mirrors](docs/native-mirrors.md#the-receiver-lock-protects-the-default-branch-only) |
| CI and Dependabot switched off on every copy, with no way to switch them on | built | `src/native.rs` of the [cghb](https://git.corbet.ch/corbet-libre/cghb) repository test `actions_and_dependabot_are_switched_off_with_no_way_to_keep_them_on`; `cfrg/tests/it/zero_ci.rs` |
| GitHub copies fed through one paced writer with budgets, kill switch and ledger | built | [gate](docs/gate.md); tests in `cfrg-cli/tests/it/gate_*.rs` |
| Merge only exact green commits into the primary | built | [land](docs/land.md) |
| Webhook-driven long-running mode for landing and mirror verification (`cfrg serve`) | built | [serve](docs/serve.md); `cfrg/src/serve.rs` tests |
| Release publication by commit and hash | built | [release](docs/release.md) |
| Opt-in import of GitLab merge requests as WIP pull requests on the primary | built, experimental | [contribution bridge](docs/pr-bridge.md); `cfrg/src/bridge/tests.rs`, `cfrg-cli/tests/it/pr_bridge_cli.rs` |
| Projects that are not ours are never mirrored or relayed | built | [contributed](docs/contributed.md); `cfrg/src/contributed.rs` |
| Issues, comments and pull or merge requests relayed between Forgejo, GitHub and GitLab | built | [relay](docs/relay.md); `cfrg/src/relay/tests.rs`, `cfrg/src/relay/tests_contrib.rs` |
| Outside contributions held on Forgejo behind an abuse screen, with one credited notice on the primary | built | [relay](docs/relay.md); `screen` in the [configuration](docs/configuration.md#relay-configuration) |
| `cfrg take-in` of a reviewed outside contribution (fast-forward or merge, never rebase or squash) | built | [take-in](docs/take-in.md); `cfrg-cli/src/take_in.rs` tests |
| Signed receiver `cfrg-serve` for Forgejo, GitHub and GitLab events, with replay protection | built | [configuration](docs/configuration.md#serve-configuration-cfrg-serve); `cfrg-serve/tests/receiver.rs` |
| Release file upload to GitHub and GitLab copies, against a checksum contract | built | [release upload](docs/release-upload.md); `cfrg-serve/tests/release.rs` |
| Wiki push to GitLab and public GitHub copies, fast-forward only | built | [native mirrors](docs/native-mirrors.md); `cfrg/src/native/wiki_sync.rs` |
| Copy settings, member plan (read or triage only, plan only), default-branch protection check; mirror ownership derived from Forgejo | built | [native mirrors](docs/native-mirrors.md); `cfrg/src/native/settings.rs`, `cfrg/src/copy_access.rs` |
| `cfrg daily`: one run for a scheduler (drift reconcile, copy settings, wiki, relay sweep, key renewal hook) | built; the key renewal step runs a script you provide | [configuration](docs/configuration.md#daily-run-cfrg-daily); `cfrg-cli/src/daily.rs` tests |
| Nix flake with the binaries, a container image of the receiver and a NixOS module | built | [deployment](docs/deployment.md); `flake.nix` |
| Prebuilt release binaries | not provided | build from the flake or from source |

What it does not do: it does not run CI or any check executor, it does not
schedule itself (schedulers dispatch; [`cfrg-serve`](docs/quickstart.md) reacts to
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
   Outside contributions: relayed as credited requests plus one notice on Forgejo,
   taken in only after review (cfrg take-in).
```

## Quick start

The [quick start](docs/quickstart.md) runs end to end from the flake alone: it builds `cfrg-serve`, writes a minimal
configuration, starts the receiver against a throwaway Forgejo repository and a throwaway GitLab project, delivers a
signed `release` event and shows the release arriving on the GitLab copy, then deletes both throwaways. About ten
commands:

```sh
curl -fsSLO https://git.corbet.ch/corbet-cid/cfrg/raw/branch/main/docs/examples/quickstart.sh
export FORGE_URL=https://forge.example.org FORGE_TOKEN=... GITLAB_TOKEN=... GITLAB_PATH=your-gitlab-user
nix shell git+https://git.corbet.ch/corbet-cid/cfrg#cfrg-serve nixpkgs#curl nixpkgs#git nixpkgs#jq nixpkgs#openssl \
  --command bash quickstart.sh
```

For the mirrors themselves, `cfrg` reads one placement file ([configuration](docs/configuration.md#placement-fields)):

```sh
nix build git+https://git.corbet.ch/corbet-cid/cfrg#cfrg
export FORGE_TOKEN=...                   # Forgejo token, read from the environment only
export GITLAB_TOKEN=...
cfrg native --placement placement.json --state native-state.json                      # plan (the default operation): writes nothing
cfrg native --placement placement.json --state native-state.json --operation status   # read-only status
cfrg native --placement placement.json --state native-state.json --operation reconcile --apply
```

The default operation is `plan` (`cfrg-cli/src/native.rs`, `default_value = "plan"`); changes are made only with
`--operation reconcile --apply`. A minimal placement is in the [configuration reference](docs/configuration.md).

## The driver repositories

The code that speaks to each forge lives in its own public repository, depended on by pinned git revision
(`[workspace.dependencies]` in `Cargo.toml`):

| Crate | Forge | Repository |
|---|---|---|
| `cfgj` | Forgejo and Codeberg | <https://git.corbet.ch/corbet-libre/cfgj> |
| `cghb` | GitHub | <https://git.corbet.ch/corbet-libre/cghb> |
| `cglb` | GitLab | <https://git.corbet.ch/corbet-libre/cglb> |
| `cbkt` | Bitbucket | <https://git.corbet.ch/corbet-libre/cbkt> |

Each is licensed LGPL-3.0-only WITH LGPL-3.0-linking-exception, builds and tests on its own with `cargo test` (no network, no
credentials), and keeps every rule specific to its forge so that the planners in this repository stay forge-independent.
Issues and pull requests for a driver go to that driver's repository.

## Documentation

| Page | Contents |
|---|---|
| [Why we built it](docs/why-we-built-it.md) | the short story, fit for an announcement |
| [Quick start](docs/quickstart.md) | build, configure and run `cfrg-serve` against throwaway repositories |
| [Concepts](docs/concepts.md) | primary, copy, placement, drivers, relay, receiver, contributed projects |
| [Configuration](docs/configuration.md) | serve, relay, placement and daily formats, credential file names |
| [Deployment](docs/deployment.md) | Nix flake, container image, source |
| [Security model](docs/security.md) | zero CI on copies, credentials, take-in after review |
| Per-feature pages | [relay](docs/relay.md), [take-in](docs/take-in.md), [release upload](docs/release-upload.md), [native mirrors](docs/native-mirrors.md), [gate](docs/gate.md), [land](docs/land.md), [serve](docs/serve.md), [release](docs/release.md), [switch](docs/switch.md), [access sync](docs/access-sync.md), [collect](docs/collect.md), [contents](docs/contents.md), [observe](docs/observe.md), [native status](docs/native-status.md), [repository policy](docs/repository-policy.md), [contribution bridge](docs/pr-bridge.md), [contributed](docs/contributed.md) |

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
