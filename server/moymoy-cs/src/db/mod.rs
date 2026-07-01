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
/// Current schema version. Bump + add a step in [`migrate`] for changes.
const SCHEMA_VERSION: i64 = 4;

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
    // Future: `if version < 5 { let tx = conn.transaction()?; tx.execute_batch(SCHEMA_V5)?;
    //          tx.pragma_update(None, "user_version", 5)?; tx.commit()?; version = 5; }`
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
