//! [`Store`] over PostgreSQL (the pgEdge database `forge_bridge`, Bridge design §5).
//!
//! Keys are UUIDs the writer makes, never sequences. Each row is the key columns plus the full row as JSONB, and
//! every write is `INSERT … ON CONFLICT DO UPDATE` of the whole row, so a retried job changes nothing. The lease is
//! one conditional upsert. A transaction is `BEGIN`/`COMMIT` on the one connection.
//!
//! A non-local host is reached over verified TLS by default, see [`super::pgtls`]; plain TCP is for localhost.

use super::model::{
    CommentRow, Hold, Intent, IntentStatus, LedgerEntry, LogicalObject, SiblingRow,
};
use super::store::{comment_order, sibling_order, Store};
use crate::{model::Forge, Result};
use postgres::{config::SslMode, types::ToSql, Client, NoTls};
use serde::{de::DeserializeOwned, Serialize};
use std::cell::RefCell;
use uuid::Uuid;

const SCHEMA_LOCK: i64 = 0x6366_7267_7265_6c61;

/// Created on connect; every statement is idempotent.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS relay_leases (lid uuid PRIMARY KEY, holder text NOT NULL, until bigint NOT NULL);
CREATE TABLE IF NOT EXISTS relay_objects (lid uuid PRIMARY KEY, doc jsonb NOT NULL);
CREATE TABLE IF NOT EXISTS relay_siblings (
    forge text NOT NULL, repo text NOT NULL, number bigint NOT NULL, lid uuid NOT NULL, doc jsonb NOT NULL,
    PRIMARY KEY (forge, repo, number));
CREATE INDEX IF NOT EXISTS relay_siblings_lid ON relay_siblings (lid);
CREATE TABLE IF NOT EXISTS relay_comments (lcid uuid PRIMARY KEY, lid uuid NOT NULL, doc jsonb NOT NULL);
CREATE INDEX IF NOT EXISTS relay_comments_lid ON relay_comments (lid);
CREATE TABLE IF NOT EXISTS relay_intents (id uuid PRIMARY KEY, lid uuid NOT NULL, planned boolean NOT NULL, doc jsonb NOT NULL);
CREATE INDEX IF NOT EXISTS relay_intents_lid ON relay_intents (lid);
CREATE TABLE IF NOT EXISTS relay_ledger (id uuid PRIMARY KEY, ts bigint NOT NULL, doc jsonb NOT NULL);
CREATE INDEX IF NOT EXISTS relay_ledger_ts ON relay_ledger (ts);
CREATE TABLE IF NOT EXISTS relay_pace (forge text NOT NULL, namespace text NOT NULL, next_any bigint NOT NULL, next_create bigint NOT NULL, PRIMARY KEY (forge, namespace));
CREATE TABLE IF NOT EXISTS relay_holds (forge text NOT NULL, namespace text NOT NULL, doc jsonb NOT NULL, PRIMARY KEY (forge, namespace));
CREATE TABLE IF NOT EXISTS relay_copy_ids (forge text NOT NULL, path text NOT NULL, repo_id text NOT NULL, resolved_at bigint NOT NULL, PRIMARY KEY (forge, path));
CREATE TABLE IF NOT EXISTS release_intents (repo text NOT NULL, tag text NOT NULL, since bigint NOT NULL, PRIMARY KEY (repo, tag));
";

pub struct PgStore {
    client: RefCell<Client>,
    depth: usize,
}

fn key(text: &str) -> Option<Uuid> {
    Uuid::parse_str(text).ok()
}

fn int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn json<T: Serialize>(row: &T) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(row)?)
}

impl PgStore {
    /// Connects with a libpq-style URL or key/value string and makes sure the schema exists.
    pub fn connect(config: &str) -> Result<Self> {
        let parsed = super::pgtls::parse(config)?;
        let mut cfg: postgres::Config = parsed.config.parse()?;
        let mut client = match parsed.mode {
            super::pgtls::Mode::Disable => cfg.ssl_mode(SslMode::Disable).connect(NoTls)?,
            // Require, so a server that refuses TLS is an error and never a silent plain connection.
            super::pgtls::Mode::Verify => cfg
                .ssl_mode(SslMode::Require)
                .connect(super::pgtls::connector(parsed.root_cert.as_deref())?)?,
        };
        // Concurrent first starts would race inside CREATE TABLE IF NOT EXISTS; one advisory lock serialises them.
        client.batch_execute(&format!(
            "SELECT pg_advisory_xact_lock({SCHEMA_LOCK}); {SCHEMA}"
        ))?;
        Ok(Self {
            client: RefCell::new(client),
            depth: 0,
        })
    }

    /// Release upload journal: an intent is written before the upload and cleared once the copies are synced, so a restart
    /// that cuts an upload off leaves it open for the next start to finish (the upload is idempotent).
    pub fn release_intent_open(&self, repo: &str, tag: &str, since: u64) -> Result<()> {
        self.run(
            "INSERT INTO release_intents (repo, tag, since) VALUES ($1, $2, $3) ON CONFLICT (repo, tag) DO NOTHING",
            &[&repo, &tag, &int(since)],
        )?;
        Ok(())
    }

    pub fn release_intent_clear(&self, repo: &str, tag: &str) -> Result<()> {
        self.run(
            "DELETE FROM release_intents WHERE repo = $1 AND tag = $2",
            &[&repo, &tag],
        )?;
        Ok(())
    }

    /// Every release intent still open, oldest first.
    pub fn release_intents_open(&self) -> Result<Vec<(String, String)>> {
        let rows = self.client.borrow_mut().query(
            "SELECT repo, tag FROM release_intents ORDER BY since, repo, tag",
            &[],
        )?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    fn run(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
        Ok(self.client.borrow_mut().execute(sql, params)?)
    }

    fn docs<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<T>> {
        let rows = self.client.borrow_mut().query(sql, params)?;
        rows.iter()
            .map(|row| Ok(serde_json::from_value(row.get::<_, serde_json::Value>(0))?))
            .collect()
    }

    fn upsert(
        &self,
        table: &str,
        columns: &str,
        values: &str,
        conflict: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<()> {
        self.run(
            &format!(
                "INSERT INTO {table} ({columns}) VALUES ({values}) ON CONFLICT ({conflict}) DO UPDATE SET \
                 doc = EXCLUDED.doc{}",
                if table == "relay_intents" { ", planned = EXCLUDED.planned" } else { "" }
            ),
            params,
        )?;
        Ok(())
    }
}

impl Store for PgStore {
    fn atomically(&mut self, f: &mut dyn FnMut(&mut dyn Store) -> Result<()>) -> Result<()> {
        let outer = self.depth == 0;
        if outer {
            self.client.borrow_mut().batch_execute("BEGIN")?;
        }
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        if outer {
            let end = if result.is_ok() { "COMMIT" } else { "ROLLBACK" };
            let ended = self.client.borrow_mut().batch_execute(end);
            result?;
            ended?;
            return Ok(());
        }
        result
    }

    fn lease(&mut self, lid: &str, holder: &str, now: u64, secs: u64) -> Result<bool> {
        let Some(lid) = key(lid) else {
            return Ok(false);
        };
        let rows = self.client.borrow_mut().query(
            "INSERT INTO relay_leases (lid, holder, until) VALUES ($1, $2, $3) \
             ON CONFLICT (lid) DO UPDATE SET holder = EXCLUDED.holder, until = EXCLUDED.until \
             WHERE relay_leases.holder = EXCLUDED.holder OR relay_leases.until <= $4 RETURNING holder",
            &[&lid, &holder, &int(now.saturating_add(secs)), &int(now)],
        )?;
        Ok(!rows.is_empty())
    }

    fn unlease(&mut self, lid: &str, holder: &str) -> Result<()> {
        if let Some(lid) = key(lid) {
            self.run(
                "DELETE FROM relay_leases WHERE lid = $1 AND holder = $2",
                &[&lid, &holder],
            )?;
        }
        Ok(())
    }

    fn object(&self, lid: &str) -> Result<Option<LogicalObject>> {
        let Some(lid) = key(lid) else { return Ok(None) };
        Ok(self
            .docs("SELECT doc FROM relay_objects WHERE lid = $1", &[&lid])?
            .pop())
    }

    fn objects(&self) -> Result<Vec<LogicalObject>> {
        self.docs("SELECT doc FROM relay_objects", &[])
    }

    fn put_object(&mut self, object: LogicalObject) -> Result<()> {
        let lid = key(&object.lid).ok_or("a logical object id must be a UUID")?;
        self.upsert(
            "relay_objects",
            "lid, doc",
            "$1, $2",
            "lid",
            &[&lid, &json(&object)?],
        )
    }

    fn sibling_at(&self, forge: Forge, repo: &str, number: u64) -> Result<Option<SiblingRow>> {
        Ok(self
            .docs(
                "SELECT doc FROM relay_siblings WHERE forge = $1 AND repo = $2 AND number = $3",
                &[&forge.as_str(), &repo, &int(number)],
            )?
            .pop())
    }

    fn siblings_of(&self, lid: &str) -> Result<Vec<SiblingRow>> {
        let Some(lid) = key(lid) else {
            return Ok(Vec::new());
        };
        let mut rows: Vec<SiblingRow> =
            self.docs("SELECT doc FROM relay_siblings WHERE lid = $1", &[&lid])?;
        rows.sort_by_key(sibling_order);
        Ok(rows)
    }

    fn put_sibling(&mut self, sibling: SiblingRow) -> Result<()> {
        let lid = key(&sibling.lid).ok_or("a logical object id must be a UUID")?;
        self.upsert(
            "relay_siblings",
            "forge, repo, number, lid, doc",
            "$1, $2, $3, $4, $5",
            "forge, repo, number",
            &[
                &sibling.forge.as_str(),
                &sibling.repo,
                &int(sibling.number),
                &lid,
                &json(&sibling)?,
            ],
        )
    }

    fn comments_of(&self, lid: &str) -> Result<Vec<CommentRow>> {
        let Some(lid) = key(lid) else {
            return Ok(Vec::new());
        };
        let mut rows: Vec<CommentRow> =
            self.docs("SELECT doc FROM relay_comments WHERE lid = $1", &[&lid])?;
        rows.sort_by_key(comment_order);
        Ok(rows)
    }

    fn put_comment(&mut self, comment: CommentRow) -> Result<()> {
        let (lcid, lid) = (key(&comment.lcid), key(&comment.lid));
        let (Some(lcid), Some(lid)) = (lcid, lid) else {
            return Err("a comment id must be a UUID".into());
        };
        self.upsert(
            "relay_comments",
            "lcid, lid, doc",
            "$1, $2, $3",
            "lcid",
            &[&lcid, &lid, &json(&comment)?],
        )
    }

    fn put_intent(&mut self, intent: Intent) -> Result<()> {
        let (id, lid) = (key(&intent.id), key(&intent.lid));
        let (Some(id), Some(lid)) = (id, lid) else {
            return Err("an intent id must be a UUID".into());
        };
        let planned = intent.status == IntentStatus::Planned;
        self.upsert(
            "relay_intents",
            "id, lid, planned, doc",
            "$1, $2, $3, $4",
            "id",
            &[&id, &lid, &planned, &json(&intent)?],
        )
    }

    fn intents_of(&self, lid: &str) -> Result<Vec<Intent>> {
        let Some(lid) = key(lid) else {
            return Ok(Vec::new());
        };
        self.docs(
            "SELECT doc FROM relay_intents WHERE lid = $1 ORDER BY (doc->>'created')::bigint, id",
            &[&lid],
        )
    }

    fn intents_open(&self) -> Result<Vec<Intent>> {
        self.docs("SELECT doc FROM relay_intents WHERE planned", &[])
    }

    fn ledger_append(&mut self, entry: LedgerEntry) -> Result<()> {
        self.upsert(
            "relay_ledger",
            "id, ts, doc",
            "$1, $2, $3",
            "id",
            &[&Uuid::new_v4(), &int(entry.ts), &json(&entry)?],
        )
    }

    fn ledger_since(&self, ts: u64) -> Result<Vec<LedgerEntry>> {
        self.docs("SELECT doc FROM relay_ledger WHERE ts >= $1", &[&int(ts)])
    }

    fn pace_claim(
        &mut self,
        forge: Forge,
        namespace: &str,
        now: u64,
        gap: u64,
        create_gap: Option<u64>,
    ) -> Result<u64> {
        // One atomic statement on the namespace row: concurrent jobs queue behind each other's claims.
        let rows = self.client.borrow_mut().query(
            "INSERT INTO relay_pace (forge, namespace, next_any, next_create) \
             VALUES ($1, $2, $3::bigint + $4::bigint, CASE WHEN $6 THEN $3::bigint + $5::bigint ELSE 0 END) \
             ON CONFLICT (forge, namespace) DO UPDATE SET \
               next_any = GREATEST($3::bigint, relay_pace.next_any, \
                                   CASE WHEN $6 THEN relay_pace.next_create ELSE 0 END) + $4::bigint, \
               next_create = CASE WHEN $6 THEN GREATEST($3::bigint, relay_pace.next_any, relay_pace.next_create) \
                                       + $5::bigint ELSE relay_pace.next_create END \
             RETURNING next_any",
            &[
                &forge.as_str(),
                &namespace,
                &int(now),
                &int(gap),
                &int(create_gap.unwrap_or(0)),
                &create_gap.is_some(),
            ],
        )?;
        let next: i64 = rows.first().ok_or("pace claim returned no row")?.get(0);
        Ok(u64::try_from(next)
            .unwrap_or(0)
            .saturating_sub(gap)
            .saturating_sub(now))
    }

    fn holds(&self) -> Result<Vec<Hold>> {
        self.docs("SELECT doc FROM relay_holds", &[])
    }

    fn hold_put(&mut self, hold: Hold) -> Result<bool> {
        let changed = self.run(
            "INSERT INTO relay_holds (forge, namespace, doc) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            &[&hold.forge.as_str(), &hold.namespace, &json(&hold)?],
        )?;
        Ok(changed > 0)
    }

    fn hold_release(&mut self, forge: Forge, namespace: &str) -> Result<bool> {
        let changed = self.run(
            "DELETE FROM relay_holds WHERE forge = $1 AND namespace = $2",
            &[&forge.as_str(), &namespace],
        )?;
        Ok(changed > 0)
    }
}

impl super::copyid::CopyIds for PgStore {
    fn load(&mut self) -> Result<std::collections::BTreeMap<(Forge, String), String>> {
        let rows = self
            .client
            .borrow_mut()
            .query("SELECT forge, path, repo_id FROM relay_copy_ids", &[])?;
        let mut out = std::collections::BTreeMap::new();
        for row in &rows {
            if let Ok(forge) = Forge::parse(row.get::<_, &str>(0)) {
                out.insert((forge, row.get::<_, String>(1)), row.get::<_, String>(2));
            }
        }
        Ok(out)
    }

    fn put(&mut self, forge: Forge, path: &str, repo_id: &str, now: u64) -> Result<()> {
        self.run(
            "INSERT INTO relay_copy_ids (forge, path, repo_id, resolved_at) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (forge, path) DO UPDATE SET repo_id = EXCLUDED.repo_id, resolved_at = EXCLUDED.resolved_at",
            &[&forge.as_str(), &path, &repo_id, &int(now)],
        )?;
        Ok(())
    }

    fn drop_id(&mut self, forge: Forge, repo_id: &str) -> Result<()> {
        self.run(
            "DELETE FROM relay_copy_ids WHERE forge = $1 AND repo_id = $2",
            &[&forge.as_str(), &repo_id],
        )?;
        Ok(())
    }
}
