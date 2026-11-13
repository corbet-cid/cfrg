# cfrg relay

The issue relay of the bridge (Bridge design §3.1, §3.2, §5; operating model R37, R38, R41, R50, R51). Code:
`cfrg/src/relay/`, library: `cfrg-relay/`, command: `cfrg-cli/src/relay.rs`. Forgejo Actions runs it; it has no queue, timer, catch-up or
rebuild of its own (R30, R49).

## Commands

| Command | What it does | Exit |
|---|---|---|
| `cfrg relay apply <event>` | Applies one event (a hint: forge, repository id, kind, number, action, actor). The event is the JSON text, a file, or `-` for standard input. Everything else is read live from the forges (RD1). | 0 for converged, pending, held and dropped; non-zero when nothing could be done now (busy lease, outage, kill switch), so the job is re-run |
| `cfrg relay sweep [--older-than s]` | Re-drives each object whose intent stayed pending longer than the bound (default 1 h), once, at most `sweep_max_objects`. Nothing pending means no read and no write. | 0 |

Both take `--config` (`CFRG_RELAY_CONFIG`, the relay configuration JSON) and `--placement` (`CFRG_PLACEMENT`, whose
`contributed` list is asked first). The database is the libpq connection string in `CFRG_RELAY_DATABASE`; it carries a
credential, so it is never an argument.

## Repositories and credentials

**The repository set** is derived from the placement on every run (`relay::derive`), never from a hand list; a `repos`
list in the relay configuration is refused. A repository is relayed when it is a Forgejo primary with at least one
GitHub or GitLab copy. Contributed projects (`native.contributed`, R10, R11) and Bitbucket (git mirror only) never take
part. A copy is a member only when the placement knows its immutable `repository_id`; the Forgejo primary is addressed
by its `source_id`. The relay identities (`relay`: the user ids of `corbet-forge` per forge) stay in the configuration;
while a forge has members but no identity the run refuses: `sweep` logs `relay: refused: ...` and exits 0, `apply`
exits non-zero.

**Credentials** are read only from files of one credentials directory: `--credentials-dir` (library: `Settings::credentials_dir`),
default `$CREDENTIALS_DIRECTORY`. In production that is a Kubernetes Secret volume of the bridge pods (or a systemd unit's
credential directory). No environment variable, no fallback to a personal token. A missing, unreadable or empty file means
that forge has no adapter: one line `relay: <forge> refused: credential <names> is not in the credentials directory` goes to
standard error, no request is made to it, and the engine treats it as an outage (the sweep exits 0, an apply exits non-zero).

| Forge | Credential file(s) |
|---|---|
| Forgejo (the placement source) | `forgejo-token` |
| GitLab (`gitlab.com`) | `gitlab-token` |
| GitHub | `github-app-id`, `github-installation-id` (decimal numbers), `github-app-key` (the PEM private key of the App) |

**Library API** (crate `cfrg-relay`; `cfrg relay` and `cfrg serve` run the same code):

```rust
pub struct Settings { config: PathBuf, placement: PathBuf, database: String /* libpq string */, credentials_dir: Option<PathBuf> }
pub fn apply(&Settings, &Hint) -> cfrg::Result<Applied>;      // Applied::Refused(why) | Applied::Done(Report)
pub fn sweep(&Settings, Option<u64>) -> cfrg::Result<Swept>;  // Swept::Refused(why)   | Swept::Done(SweepReport)
```

`Refused` means the configuration is not ready (no relay identity ids yet): nothing was read or written. In `Done`,
`report.outcome == Outcome::Deferred(_)` means nothing could be done now (busy lease, outage, kill switch): retry later.
Both functions BLOCK (database, HTTP through a private current-thread runtime): from async code call them with
`tokio::task::spawn_blocking`, never on a runtime thread. Every call reads the configuration and placement again, connects
the store first (a refused call still proves the database login) and builds the forge adapters from the credentials directory.

The process owns one current-thread tokio runtime; the synchronous engine calls the async drivers through it.

**Driver coverage.** The B3 drivers write (create an issue, comment, add labels, close). The adapters answer every other
engine call (`read`, `update_comment`, deletions, marker search; a patch that edits text, reopens or removes labels)
with `Unavailable`, so the intent stays pending and nothing is written twice. The read side is the next driver task.

## Order of an `apply`

1. Policy: the repository must be listed, the actor must not be the relay identity (RD4a).
2. `Contributed::refuse` for the primary and every copy of the set (R10, R11): no read and no write follows a refusal.
3. The single-writer lease of the logical object (a UUID derived from the origin) is taken. A busy lease defers.
4. Read every sibling, merge (RD3), write the difference hub first (RD2), each write behind an intent row.

## Store (pgEdge database `forge_bridge`)

Created on connect with `CREATE … IF NOT EXISTS` (`PgStore::SCHEMA`). Keys are UUIDs made by the writer, no sequences;
each row is key columns plus the full row as JSONB, written by `INSERT … ON CONFLICT DO UPDATE`.

| Table | Key | Holds |
|---|---|---|
| `relay_leases` | `lid` | `holder`, `until`: one conditional upsert takes or renews it, an expired one is taken over |
| `relay_objects` | `lid` | the logical object: origin, canon, hold, tombstone (a late event cannot resurrect a deleted object) |
| `relay_siblings` | `(forge, repo, number)` | the link to its object, snapshot, `deleted` |
| `relay_comments` | `lcid` | an origin comment and its copies |
| `relay_intents` | `id` | the journal: written before every provider write, settled after |
| `relay_ledger` | `id` | counted writes, fan-outs and cascaded deletions (the caps) |
| `relay_holds` | `(forge, namespace)` | a refused namespace; only the owner lifts it |

| `relay_pace` | `(forge, namespace)` | the next allowed write and the next allowed creation (the gaps) |

A host that is not local is reached over TLS (rustls through `tokio-postgres-rustls`) with the chain and the server
name verified: `verify-full` is the default there, and `require` and `verify-ca` are treated the same (never weaker
than libpq). `sslrootcert=<pem>` names the fleet CA; without it the system roots are used. Plain TCP is only for
localhost, a loopback address or a unix socket, and `sslmode=disable` on any other host is an error.

## Limits

| Limit | Default |
|---|---|
| deletions cascaded per hour, fleet-wide / per repository | 10 / 3 (otherwise the deletion is held for the reviewer) |
| new objects fanned out per author per day | 5 (R51; the rest stays on Forgejo) |
| writing passes per object per hour | 30 (RF8 backstop: the object is frozen) |
| mutations per namespace per hour / day | 60 / 400 (RD11) |
| gap between writes / between object creations, per namespace | 2 s / 12 s (`pacing.write_gap_secs`, `create_gap_secs`) |
| lease | 600 s |

The gaps hold across concurrent jobs: before a write the job claims its slot in `relay_pace` with one atomic upsert
(the store returns the wait), sleeps once for that wait and then writes. There is no polling loop.

## Tests

`cargo test -p cfrg --lib relay` runs the fake-forge suite and the in-memory store. The PostgreSQL tests are ignored
unless a throwaway server is named:
`CFRG_RELAY_TEST_PG='host=/tmp/sock user=postgres dbname=postgres' cargo test -p cfrg --lib relay::tests_pg -- --ignored`
(and `-p cfrg-cli --test it relay_cli -- --ignored`). The TLS and database pacing tests start their own server with a
throwaway CA: `CFRG_RELAY_TEST_PG_BIN=<dir with initdb and pg_ctl> cargo test -p cfrg --lib relay::tests_tls -- --ignored`.

## What a driver implements

`relay::RouterForge` (in `cfrg/src/relay/forge.rs`): `read`, `create_object`, `update_object`, `create_comment`,
`update_comment`, `delete_comment`, `delete_object`, `find_by_marker`, `is_maintainer`. A definitive 404 or 410 is
`ReadOutcome::Gone`; a 403, 5xx, timeout or rename is an error, never `Gone`.
