# cfrg

Forge placement, reconciliation, status, bridge, evidence collection and access
mirroring. Extracted from [ccid](https://git.corbet.ch/corbet-libs/ccid);
ccid keeps check execution and now calls this crate where it needs forge logic.

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
cfrg access plan --identities identities.toml --baseline state.json \
  --observed github=snap.json
```

See [repository placement](docs/repository-policy.md),
[reconciliation](docs/forge-sync.md), [native statuses](docs/native-status.md),
[contribution bridge](docs/pr-bridge.md), [evidence collection](docs/collect.md)
and [access mirroring](docs/access-sync.md).

The Rust library forbids unsafe code. `CFRG_SOURCE_REVISION` is embedded at
build time like ccid's revision. No scheduler, check executor or service is
included: cfrg reports, reconciles and collects; schedulers dispatch.

## License

Functional Source License, Version 1.1, ALv2 Future License (like ccid).
The full text is in `LICENSE.md`.
