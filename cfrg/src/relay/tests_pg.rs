//! The store contract and the whole relay against a real PostgreSQL. Ignored unless `CFRG_RELAY_TEST_PG` holds a
//! libpq key/value connection string of a throwaway server, e.g. `host=/tmp/sock user=postgres dbname=postgres`.
//! Run: `CFRG_RELAY_TEST_PG=... cargo test -p cfrg --lib relay::tests_pg -- --ignored`.

use super::model::{Hold, LogicalObject};
use super::pg::PgStore;
use super::store::Store;
use super::tests::{alice, config, GH, GL, H};
use super::Outcome;
use crate::model::Forge;
use postgres::{Client, NoTls};
use std::sync::atomic::{AtomicU32, Ordering};

static N: AtomicU32 = AtomicU32::new(0);

/// A store on its own schema, so tests never see each other's rows.
fn config_of(schema: &str) -> String {
    let base = std::env::var("CFRG_RELAY_TEST_PG")
        .expect("CFRG_RELAY_TEST_PG is set for the ignored PostgreSQL tests");
    let mut admin = Client::connect(&base, NoTls).unwrap();
    admin
        .batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
        .unwrap();
    format!("{base} options='-c search_path={schema}'")
}

fn fresh() -> (String, PgStore) {
    let schema = format!(
        "t{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let config = config_of(&schema);
    let store = PgStore::connect(&config).unwrap();
    (config, store)
}

fn object(lid: &str) -> LogicalObject {
    let origin = super::model::ObjectRef {
        forge: Forge::Github,
        repo: "r".into(),
        kind: super::model::Kind::Issue,
        number: 1,
    };
    LogicalObject {
        lid: lid.into(),
        kind: super::model::Kind::Issue,
        primary: "a/b".into(),
        origin,
        origin_author: alice(),
        state: super::model::ObjState::Open,
        screened: true,
        held: None,
        tombstone: None,
        canon: super::model::Canon::default(),
        created: 1,
    }
}

const LID: &str = "0b9c4b0e-6d5d-8a3e-9d1c-2f3a4b5c6d7e";

#[test]
#[ignore = "needs CFRG_RELAY_TEST_PG"]
fn one_writer_holds_the_lease_until_it_expires_or_is_released() {
    let (config, mut a) = fresh();
    let mut b = PgStore::connect(&config).unwrap();
    assert!(a.lease(LID, "job-a", 100, 600).unwrap());
    assert!(!b.lease(LID, "job-b", 200, 600).unwrap(), "held");
    assert!(
        a.lease(LID, "job-a", 300, 600).unwrap(),
        "the holder renews"
    );
    assert!(
        !b.lease(LID, "job-b", 899, 600).unwrap(),
        "renewed until 900"
    );
    assert!(b.lease(LID, "job-b", 900, 600).unwrap(), "expired");
    b.unlease(LID, "job-a").unwrap();
    assert!(
        !a.lease(LID, "job-a", 901, 600).unwrap(),
        "only the holder releases"
    );
    b.unlease(LID, "job-b").unwrap();
    assert!(a.lease(LID, "job-a", 902, 600).unwrap(), "released");
}

#[test]
#[ignore = "needs CFRG_RELAY_TEST_PG"]
fn racing_writers_get_exactly_one_lease() {
    let (config, _first) = fresh();
    let wins: usize = (0..8)
        .map(|i| {
            let config = config.clone();
            std::thread::spawn(move || {
                PgStore::connect(&config)
                    .unwrap()
                    .lease(LID, &format!("job-{i}"), 1_000, 600)
                    .unwrap()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| usize::from(t.join().unwrap()))
        .sum();
    assert_eq!(wins, 1);
}

#[test]
#[ignore = "needs CFRG_RELAY_TEST_PG"]
fn upserts_are_idempotent_and_a_failed_transaction_changes_nothing() {
    let (_, mut store) = fresh();
    let mut obj = object(LID);
    store.put_object(obj.clone()).unwrap();
    store.put_object(obj.clone()).unwrap();
    assert_eq!(
        store.objects().unwrap(),
        vec![obj.clone()],
        "the same row twice is one row"
    );
    obj.created = 2;
    store.put_object(obj.clone()).unwrap();
    assert_eq!(
        store.object(LID).unwrap().unwrap().created,
        2,
        "the full row is replaced"
    );
    assert!(
        store.object("not-a-uuid").unwrap().is_none(),
        "a hostile id finds nothing"
    );

    let other = "1b9c4b0e-6d5d-8a3e-9d1c-2f3a4b5c6d7e";
    let failed = store.atomically(&mut |tx| {
        tx.put_object(object(other))?;
        Err("boom".into())
    });
    assert!(failed.is_err());
    assert!(store.object(other).unwrap().is_none(), "rolled back");
    store
        .atomically(&mut |tx| tx.put_object(object(other)))
        .unwrap();
    assert!(store.object(other).unwrap().is_some(), "committed");

    let hold = Hold {
        forge: Forge::Github,
        namespace: "org".into(),
        reason: "429".into(),
        since: 1,
    };
    assert!(store.hold_put(hold.clone()).unwrap());
    assert!(!store.hold_put(hold).unwrap());
    assert!(store.hold_release(Forge::Github, "org").unwrap());
}

#[test]
#[ignore = "needs CFRG_RELAY_TEST_PG"]
fn the_whole_relay_runs_on_postgres_and_a_restart_or_a_replay_writes_nothing() {
    let (config, store) = fresh();
    let mut h = H::on(config_with_clock(), Box::new(store));
    let n =
        h.gh.borrow_mut()
            .open_issue(GH, &alice(), "Crash", "text", &["bug"]);
    let hint = h.hint(Forge::Github, n, super::HintAction::Changed, Some(&alice()));
    assert_eq!(h.eng().apply(&hint).unwrap().outcome, Outcome::Converged);
    assert_eq!(h.gl.borrow().repos[GL].issues.len(), 1);
    // The same event again, and again after a new connection (a new job): converged, no write.
    h.clear_logs();
    assert_eq!(h.eng().apply(&hint).unwrap().writes, 0);
    h.store = Box::new(PgStore::connect(&config).unwrap());
    let again = h.eng().apply(&hint).unwrap();
    assert_eq!((again.outcome, again.writes), (Outcome::Converged, 0));
    assert!(h.mutations().is_empty(), "{:?}", h.mutations());
    assert_eq!(h.gl.borrow().repos[GL].issues.len(), 1);
    assert!(h.eng().sweep(None).unwrap().reports.is_empty());
}

fn config_with_clock() -> super::RouterConfig {
    config()
}
