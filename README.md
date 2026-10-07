# cfrg

Forge placement, reconciliation, status, bridge, evidence collection, repository
contents, observation and access mirroring. ccid keeps check execution and calls this tool
wherever it needs a forge; it never talks to a forge itself.

```sh
cfrg validate --policy forges.json
cfrg plan --policy forges.json --repository widget
cfrg decide --policy forges.json --repository widget --require linux-x86_64 \
  --observations states.json
cfrg clone --policy forges.json --repository widget --commit FULL_SHA \
  --destination NEW_DIRECTORY
cfrg sync --policy forges.json --repository widget --all-refs --to backup --apply
cfrg status --config status.toml --state-dir STATE --commit SHA \
  --name verify --url https://ci.example/run/1 --state success --started 1700000000
cfrg bridge --enable-gitlab-import --config bridge.toml --state-dir STATE
cfrg collect --forge github --output github.json
cfrg contents --forge forgejo --origin https://forge.example --token-env FORGE_TOKEN < query.json
cfrg observe --forge forgejo --origin https://forge.example --token-env FORGE_TOKEN \
  head org/name main
cfrg access plan --identities identities.toml --baseline state.json \
  --observed github=snap.json
```

See [repository placement](docs/repository-policy.md),
[reconciliation](docs/forge-sync.md), [native statuses](docs/native-status.md), [landing](docs/land.md), [serve](docs/serve.md), [release](docs/release.md),
[contribution bridge](docs/pr-bridge.md), [evidence collection](docs/collect.md),
[repository contents](docs/contents.md), [observe](docs/observe.md),
[access mirroring](docs/access-sync.md) and
[replication with any primary and the primary switch](docs/switch.md).

The Rust library forbids unsafe code. `CFRG_SOURCE_REVISION` is embedded at
build time like ccid's revision. No scheduler, check executor or service is
included: cfrg reports, reconciles and collects; schedulers dispatch.

## License

Functional Source License, Version 1.1, ALv2 Future License (like ccid).
The full text is in `LICENSE.md`.
