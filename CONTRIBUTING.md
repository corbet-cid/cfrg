# Contributing

Contact goes through the project's issue tracker. This repository publishes no
personal contact addresses.

## Where to contribute

The primary repository is on Forgejo; GitHub, GitLab and Bitbucket hold copies.
A pull or merge request on a copy is a valid way to contribute. Because copies
never run CI ([security](docs/security.md)), a maintainer takes the change into
the primary before anything runs on it: the relay shows the request on the primary as a
`contribution` notice, and after a maintainer's review `cfrg take-in` takes exactly the
reviewed commit in ([take-in](docs/take-in.md)).

## Before you send a change

* Keep the change small and one-purpose.
* Run the suite: `cargo test --workspace`.
* Format and lint: `cargo fmt --check` and `cargo clippy --workspace`.
  The workspace denies `dbg!`, `todo!` and `unimplemented!` and forbids unsafe
  code (`Cargo.toml`, `[workspace.lints]`).
* Claims in documentation must name the code or test that backs them. A feature
  that is not built is not documented as existing.
* Do not put host names, IP addresses or e-mail addresses of private systems in
  the repository. Use `example.invalid`-style placeholders.
* Commits go to `main` without feature-branch ceremony; keep history linear.
  No `Co-Authored-By` or similar credit trailers.

## AI-assistance disclosure

cfrg is built with AI assistance and reviewed by a human maintainer. Much of the
code, tests and documentation was drafted by AI models working under the
maintainer's direction; the maintainer decides design and licensing and is
responsible for what is merged. Treat the project accordingly: read the tests,
which are the evidence, and report anything that looks wrong.

Contributions made with AI assistance are accepted under the same rules as any
other, provided you understand and stand behind the change and have the right to
contribute it.

## License of contributions

By contributing you agree that your contribution is licensed under
LGPL-3.0-only WITH LGPL-3.0-linking-exception, the license of the project.
Add no file without a license; `reuse lint` must pass
(`REUSE.toml` covers the tree).
