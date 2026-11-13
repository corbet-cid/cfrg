# Changelog

All notable changes are recorded here. The format follows Keep a Changelog; the
project is pre-1.0 and versions are not yet released publicly.

## [Unreleased]

### Changed
- License changed from FSL-1.1-ALv2 to
  LGPL-3.0-only WITH LGPL-3.0-linking-exception. Licence texts in `LICENSES/`,
  per-file declarations in `REUSE.toml`, `license` field in every `Cargo.toml`.

### Added
- Publication documentation skeleton: README, `docs/concepts.md`,
  `docs/configuration.md`, `docs/deployment.md`, `docs/security.md`,
  `docs/why-we-built-it.md`, `CONTRIBUTING.md` (task B12). To be filled from
  the built code in B17.

### Planned (not built)
- Issue and pull-request relay (B2, B3, B8); receiver and release upload (B5);
  wiki push (B10); take-in (B13); binaries, container and flake (B14).

## [0.1.0]

Workspace version of the existing code (`Cargo.toml`). Contains: native push
mirrors, gate, land, serve, release, switch, access sync, collect, contents,
observe, status, and the experimental GitLab contribution bridge. No public
release has been published.
