//! SQLite engine: an r2d2 connection pool, per-connection PRAGMAs (WAL for
//! reader concurrency), and a `user_version`-stepped migration.
//!
//! axum handlers are async but rusqlite is synchronous, so DB work runs inside
//! `tokio::task::spawn_blocking` (see [`crate::api`]). Under WAL many readers run
//! concurrently with one writer; writers serialize via `BEGIN IMMEDIATE` +
//! `busy_timeout`.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OptionalExtension};

/// A pooled SQLite handle shared across async handlers (cheap to clone).
pub type Pool = r2d2::Pool<SqliteConnectionManager>;
/// One checked-out connection.
pub type PooledConn = r2d2::PooledConnection<SqliteConnectionManager>;

/// The v1 schema baseline (idempotent `CREATE IF NOT EXISTS`).
const SCHEMA_V1: &str = include_str!("schema.sql");
/// The v2 delta: independent MoyMoy accounts (handle + PIN), sessions, MC links
/// (additive ALTERs + new tables; resets legacy mc_uuid-keyed wallet data).
const SCHEMA_V2: &str = include_str!("schema_v2.sql");
/// The v3 delta: one Minecraft character belongs to exactly one MoyMoy account
/// (UNIQUE index on account_mc_links.mc_uuid).
const SCHEMA_V3: &str = include_str!("schema_v3.sql");
/// The v4 delta: verified email + OTP table (email verify / 2FA / PIN recovery).
const SCHEMA_V4: &str = include_str!("schema_v4.sql");
/// The v5 delta: character ownership moves from the `account_mc_links` table to
/// a per-request Hub-signed attestation; `emerald_ops` gains the consented
/// `attester_id` and charge idempotency becomes account-scoped.
const SCHEMA_V5: &str = include_str!("schema_v5.sql");
/// Current schema version. Bump + add a step in [`migrate`] for changes.
const SCHEMA_VERSION: i64 = 5;

/// Open (creating if absent) the SQLite DB at `path`, returning a pool whose
/// connections all have WAL + foreign keys + a busy timeout set, with the schema
/// migrated to [`SCHEMA_VERSION`].
pub fn open(path: &str) -> anyhow::Result<Pool> {
    let manager = SqliteConnectionManager::file(path).with_init(|c| {
        // Per-connection PRAGMAs. WAL is persisted at the DB level (set once here),
        // foreign_keys + busy_timeout are per-connection and must be re-applied.
        c.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA synchronous = NORMAL;\
             PRAGMA foreign_keys = ON;\
             PRAGMA busy_timeout = 5000;",
        )
    });
    let pool = r2d2::Pool::builder()
        .max_size(8)
        .build(manager)
        .map_err(|e| anyhow::anyhow!("build sqlite pool ({path}): {e}"))?;

    let mut conn = pool
        .get()
        .map_err(|e| anyhow::anyhow!("acquire sqlite connection: {e}"))?;
    migrate(&mut conn)?;
    Ok(pool)
}

/// A migrated, empty in-memory pool for unit tests.
///
/// `max_size(1)` is load-bearing, not tuning: `SqliteConnectionManager::memory()`
/// gives every connection its OWN database, so a larger pool would hand different
/// checkouts different (and unmigrated) data.
#[cfg(test)]
pub fn open_memory() -> anyhow::Result<Pool> {
    let pool = r2d2::Pool::builder()
        .max_size(1)
        .build(SqliteConnectionManager::memory())
        .map_err(|e| anyhow::anyhow!("build in-memory sqlite pool: {e}"))?;
    let mut conn = pool.get()?;
    migrate(&mut conn)?;
    drop(conn);
    Ok(pool)
}

/// Step the schema forward by `PRAGMA user_version`. Each version is applied in
/// its own transaction; a fresh DB (version 0) gets the v1 baseline.
fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    let mut version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        anyhow::bail!(
            "database user_version {version} is newer than this binary supports \
             ({SCHEMA_VERSION}) — refusing to run against a future schema"
        );
    }
    if version < 1 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V1)?;
        // Stamp version inside the same tx so a crash between commit and the
        // old out-of-tx pragma_update can't leave the DB in a re-migratable state.
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
        version = 1;
        tracing::info!("sqlite migrated to schema v1");
    }
    if version < 2 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V2)?;
        tx.pragma_update(None, "user_version", 2)?;
        tx.commit()?;
        version = 2;
        tracing::info!("sqlite migrated to schema v2 (independent MoyMoy accounts)");
    }
    if version < 3 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V3)?;
        tx.pragma_update(None, "user_version", 3)?;
        tx.commit()?;
        version = 3;
        tracing::info!("sqlite migrated to schema v3 (one character ↔ one account)");
    }
    if version < 4 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V4)?;
        tx.pragma_update(None, "user_version", 4)?;
        tx.commit()?;
        version = 4;
        tracing::info!("sqlite migrated to schema v4 (email verification / 2FA / recovery)");
    }
    if version < 5 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V5)?;
        tx.pragma_update(None, "user_version", 5)?;
        tx.commit()?;
        version = 5;
        tracing::info!("sqlite migrated to schema v5 (attestation-based character authorization)");
    }
    // Future: `if version < 6 { let tx = conn.transaction()?; tx.execute_batch(SCHEMA_V6)?;
    //          tx.pragma_update(None, "user_version", 6)?; tx.commit()?; version = 6; }`
    tracing::debug!(schema_version = version, "sqlite schema current");
    Ok(())
}

/// Look up a frozen idempotency response by (key, scope) — the stored JSON
/// string. `scope` is part of the match so the same key reused across a `send`
/// and a `pay` cannot replay the wrong operation's response.
pub fn idem_get(conn: &Connection, key: &str, scope: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT response_json FROM idempotency WHERE idem_key = ?1 AND scope = ?2",
        [key, scope],
        |r| r.get::<_, String>(0),
    )
    .optional()
}

/// Freeze an idempotency response so a retry of the same key replays it.
/// `INSERT OR IGNORE` so a race never overwrites a committed outcome.
pub fn idem_put(conn: &Connection, key: &str, scope: &str, json: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO idempotency (idem_key, scope, response_json, created_unix_ms) \
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![key, scope, json, now_ms()],
    )?;
    Ok(())
}

/// Wall-clock epoch milliseconds (ledger timestamps).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// True when `path` (and its `-wal`/`-shm` siblings) can be created — used only
/// for a clearer startup error than a mid-run pool failure.
pub fn ensure_parent_dir(path: &str) -> anyhow::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("create db parent dir {}: {e}", parent.display()))?;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A connection at exactly schema v4 — the state a deployed wallet is in
    /// before this build touches it.
    fn v4_db() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        for (sql, v) in [
            (SCHEMA_V1, 1),
            (SCHEMA_V2, 2),
            (SCHEMA_V3, 3),
            (SCHEMA_V4, 4),
        ] {
            conn.execute_batch(sql).expect("schema step applies");
            conn.pragma_update(None, "user_version", v).unwrap();
        }
        conn
    }

    /// The bundled SQLite is built with foreign keys ON by default, so every
    /// `account_id` a fixture references has to exist.
    fn insert_account(conn: &Connection, account_id: &str) {
        conn.execute(
            "INSERT INTO accounts (account_id, created_unix_ms, updated_unix_ms) VALUES (?1, 0, 0)",
            [account_id],
        )
        .unwrap();
    }

    fn insert_op(conn: &Connection, op_id: &str, idem: &str, account: &str, state: &str) {
        conn.execute(
            "INSERT INTO emerald_ops (op_id, idem_key, account_id, mc_uuid, direction, \
               requested_amount, settled_amount, state, created_unix_ms, updated_unix_ms) \
             VALUES (?1, ?2, ?3, 'uuid-1', 'charge', 10, NULL, ?4, 1, 1)",
            rusqlite::params![op_id, idem, account, state],
        )
        .unwrap();
    }

    fn state_of(conn: &Connection, op_id: &str) -> String {
        conn.query_row(
            "SELECT state FROM emerald_ops WHERE op_id = ?1",
            [op_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn migration_v4_to_v5() {
        let mut conn = v4_db();
        insert_account(&conn, "acct-a");
        insert_account(&conn, "acct-b");
        conn.execute(
            "INSERT INTO account_mc_links (account_id, mc_uuid, mcid, linked_unix_ms) \
             VALUES ('acct-a', 'uuid-1', 'Steve', 1)",
            [],
        )
        .unwrap();
        insert_op(&conn, "op-pending", "k-pending", "acct-a", "pending");
        insert_op(&conn, "op-sent", "k-sent", "acct-a", "sent");
        insert_op(&conn, "op-settled", "k-settled", "acct-b", "settled");
        for key in ["k-pending", "k-sent", "k-settled"] {
            idem_put(&conn, key, "charge", "{\"ok\":true}").unwrap();
        }
        // A send-scoped row and an orphan charge row: neither must be re-scoped.
        idem_put(&conn, "k-send", "send", "{\"ok\":true}").unwrap();
        idem_put(&conn, "k-orphan", "charge", "{\"ok\":true}").unwrap();

        migrate(&mut conn).expect("v4 → v5");

        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            5
        );
        // The link table is gone — no residue left to read as an ownership key.
        let links: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='account_mc_links'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(links, 0);

        // The consented-server column exists and is NULL for every pre-v5 op.
        let null_attesters: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM emerald_ops WHERE attester_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(null_attesters, 3);

        // Non-terminal ops are terminated, and asymmetrically: an undelivered
        // 'pending' can be failed, but a 'sent' one may have consumed and so goes
        // to 'stuck' for review rather than being written off.
        assert_eq!(state_of(&conn, "op-pending"), "failed");
        assert_eq!(state_of(&conn, "op-sent"), "stuck");
        assert_eq!(state_of(&conn, "op-settled"), "settled");

        // Charge idempotency moved into its op's account namespace…
        for (key, account) in [
            ("k-pending", "acct-a"),
            ("k-sent", "acct-a"),
            ("k-settled", "acct-b"),
        ] {
            assert!(
                idem_get(&conn, key, &format!("charge:{account}"))
                    .unwrap()
                    .is_some(),
                "{key} was not re-scoped"
            );
            assert!(idem_get(&conn, key, "charge").unwrap().is_none());
        }
        // …while other scopes and orphans are untouched.
        assert!(idem_get(&conn, "k-send", "send").unwrap().is_some());
        assert!(idem_get(&conn, "k-orphan", "charge").unwrap().is_some());
    }

    #[test]
    fn a_future_schema_is_refused_rather_than_downgraded() {
        let mut conn = v4_db();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(migrate(&mut conn).is_err());
    }
}
