# cfrg relay

The issue relay of the bridge (Bridge design §3.1, §3.2, §5; operating model R37, R38, R41, R50, R51). Code:
`cfrg/src/relay/`, command: `cfrg-cli/src/relay.rs`. Forgejo Actions runs it; it has no queue, timer, catch-up or
rebuild of its own (R30, R49).

## Commands

| Command | What it does | Exit |
|---|---|---|
| `cfrg relay apply <event>` | Applies one event (a hint: forge, repository id, kind, number, action, actor). The event is the JSON text, a file, or `-` for standard input. Everything else is read live from the forges (RD1). | 0 for converged, pending, held and dropped; non-zero when nothing could be done now (busy lease, outage, kill switch), so the job is re-run |
| `cfrg relay sweep [--older-than s]` | Re-drives each object whose intent stayed pending longer than the bound (default 1 h), once, at most `sweep_max_objects`. Nothing pending means no read and no write. | 0 |

Both take `--config` (`CFRG_RELAY_CONFIG`, the relay configuration JSON) and `--placement` (`CFRG_PLACEMENT`, whose
`contributed` list is asked first). The database is the libpq connection string in `CFRG_RELAY_DATABASE`; it carries a
credential, so it is never an argument.

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

The connection is plain TCP: `sslmode=require` fails closed. A TLS connector is open until the runners connect over a
public network.

## Limits

| Limit | Default |
|---|---|
| deletions cascaded per hour, fleet-wide / per repository | 10 / 3 (otherwise the deletion is held for the reviewer) |
| new objects fanned out per author per day | 5 (R51; the rest stays on Forgejo) |
| writing passes per object per hour | 30 (RF8 backstop: the object is frozen) |
| mutations per namespace per hour / day | 60 / 400 (RD11) |
| lease | 600 s |

## Tests

`cargo test -p cfrg --lib relay` runs the fake-forge suite and the in-memory store. The PostgreSQL tests are ignored
unless a throwaway server is named:
`CFRG_RELAY_TEST_PG='host=/tmp/sock user=postgres dbname=postgres' cargo test -p cfrg --lib relay::tests_pg -- --ignored`
(and `-p cfrg-cli --test it relay_cli -- --ignored`).

## What a driver implements

`relay::RouterForge` (in `cfrg/src/relay/forge.rs`): `read`, `create_object`, `update_object`, `create_comment`,
`update_comment`, `delete_comment`, `delete_object`, `find_by_marker`, `is_maintainer`. A definitive 404 or 410 is
`ReadOutcome::Gone`; a 403, 5xx, timeout or rename is an error, never `Gone`.
