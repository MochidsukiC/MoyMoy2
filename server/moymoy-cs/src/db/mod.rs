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
/// The v6 delta: merchants gain an owner, an API credential, a status and a
/// confusable-skeleton name claim; `payment_intents` carries EC payments.
const SCHEMA_V6: &str = include_str!("schema_v6.sql");
/// The v7 delta: deposit notifications — a per-session device link
/// (`moymoy_sessions.mochi_account_id`) and the transactional
/// `notification_outbox` wallet.rs writes inside each crediting transaction.
const SCHEMA_V7: &str = include_str!("schema_v7.sql");
/// The v8 delta: every amount column becomes an integer count of minor units
/// (1/100 エメ) — a pure ×100 rescale, including the amounts frozen inside
/// `idempotency.response_json` (rewritten in place, never deleted).
const SCHEMA_V8: &str = include_str!("schema_v8.sql");
/// The v9 delta: merchant revenue is held in a MoyMoy escrow account between
/// approval and a merchant-reported fulfilment, so there is always something to
/// refund from. Adds the escrow lifecycle columns to `payment_intents`, and
/// closes every pre-v9 `paid` row so the release sweep cannot pay it again.
const SCHEMA_V9: &str = include_str!("schema_v9.sql");
/// The v10 delta: `payment_intents.fulfil_reason` — the shop's own account of why
/// a fulfilment fell short, kept for as long as the movement it explains.
const SCHEMA_V10: &str = include_str!("schema_v10.sql");
/// The v11 delta: `accounts.stepup_verified_unix_ms` — when this account last
/// cleared a second factor, so `riskauth`'s outflow window can start there rather
/// than counting movements the holder has already authenticated.
const SCHEMA_V11: &str = include_str!("schema_v11.sql");
/// The v12 delta: `payment_intents.escrow_parked_unix_ms` — the no-report
/// deadline parks a payment for a human instead of refunding it on a timer, and
/// the release sweep's partial index excludes what it has parked.
const SCHEMA_V12: &str = include_str!("schema_v12.sql");
/// Current schema version. Bump + add a step in [`migrate`] for changes.
const SCHEMA_VERSION: i64 = 12;

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
    if version < 6 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V6)?;
        // The skeleton is computed by a Unicode table, not by SQL, so the
        // backfill of pre-v6 names lives here rather than in the .sql file — and
        // inside the SAME transaction, so a failure leaves no half-claimed names.
        backfill_merchant_skeletons(&tx)?;
        tx.pragma_update(None, "user_version", 6)?;
        tx.commit()?;
        version = 6;
        tracing::info!("sqlite migrated to schema v6 (self-serve merchants + payment intents)");
    }
    if version < 7 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V7)?;
        tx.pragma_update(None, "user_version", 7)?;
        tx.commit()?;
        version = 7;
        tracing::info!("sqlite migrated to schema v7 (deposit notifications: device links + outbox)");
    }
    if version < 8 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V8)?;
        tx.pragma_update(None, "user_version", 8)?;
        tx.commit()?;
        version = 8;
        tracing::info!("sqlite migrated to schema v8 (amounts in minor units, 1/100 エメ)");
    }
    if version < 9 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V9)?;
        tx.pragma_update(None, "user_version", 9)?;
        tx.commit()?;
        version = 9;
        tracing::info!("sqlite migrated to schema v9 (merchant revenue held in escrow)");
    }
    if version < 10 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V10)?;
        tx.pragma_update(None, "user_version", 10)?;
        tx.commit()?;
        version = 10;
        tracing::info!("sqlite migrated to schema v10 (fulfilment reasons are kept)");
    }
    if version < 11 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V11)?;
        tx.pragma_update(None, "user_version", 11)?;
        tx.commit()?;
        version = 11;
        tracing::info!("sqlite migrated to schema v11 (step-up resets the outflow window)");
    }
    if version < 12 {
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA_V12)?;
        tx.pragma_update(None, "user_version", 12)?;
        tx.commit()?;
        version = 12;
        tracing::info!("sqlite migrated to schema v12 (an unreported payment is parked, not refunded)");
    }
    // Future: `if version < 13 { let tx = conn.transaction()?; tx.execute_batch(SCHEMA_V13)?;
    //          tx.pragma_update(None, "user_version", 13)?; tx.commit()?; version = 13; }`
    tracing::debug!(schema_version = version, "sqlite schema current");
    Ok(())
}

/// Give every pre-v6 merchant the confusable skeleton its name resolves to, so
/// the new uniqueness rule covers names that existed before the rule did.
///
/// **A collision must not fail the migration.** Two merchants seeded years apart
/// can perfectly legitimately resolve to the same skeleton, and a migration that
/// refuses to apply takes the whole wallet offline at startup — a deployment
/// outage caused by a naming rule. So the loser keeps a NULL skeleton (the index
/// is partial, so that is allowed) and says so in the log: it simply does not
/// reserve its name, and an operator can rename it and restart to claim one.
///
/// Oldest first, so if this ever runs twice the same row wins both times.
fn backfill_merchant_skeletons(tx: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    let rows: Vec<(String, String)> = {
        let mut stmt =
            tx.prepare("SELECT merchant_id, name FROM merchants ORDER BY created_unix_ms ASC")?;
        let v = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        v
    };
    let mut claimed = std::collections::HashSet::new();
    for (merchant_id, name) in rows {
        let skeleton = crate::merchant::name_skeleton(&name);
        if skeleton.is_empty() || !claimed.insert(skeleton.clone()) {
            tracing::warn!(
                %merchant_id, %name, %skeleton,
                "v6 migration: this merchant's name resolves to a skeleton an older merchant \
                 already claims (or to nothing at all) — leaving name_skeleton NULL so the \
                 migration still applies; the merchant keeps working but reserves no name"
            );
            continue;
        }
        tx.execute(
            "UPDATE merchants SET name_skeleton = ?2 WHERE merchant_id = ?1",
            rusqlite::params![merchant_id, skeleton],
        )?;
    }
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

    /// A connection at exactly schema v7 — the state a deployed wallet is in
    /// before the unit migration touches it. The v6 name backfill is skipped
    /// deliberately: it is Rust, not SQL, and a synthetic DB seeds its own
    /// merchants after this returns.
    fn v7_db() -> Connection {
        let conn = v4_db();
        for (sql, v) in [(SCHEMA_V5, 5), (SCHEMA_V6, 6), (SCHEMA_V7, 7)] {
            conn.execute_batch(sql).expect("schema step applies");
            conn.pragma_update(None, "user_version", v).unwrap();
        }
        conn
    }

    /// A connection at exactly schema v8 — a wallet that has been taking payments
    /// the pre-escrow way, which is the state v9 has to convert safely.
    fn v8_db() -> Connection {
        let conn = v7_db();
        conn.execute_batch(SCHEMA_V8).expect("schema step applies");
        conn.pragma_update(None, "user_version", 8).unwrap();
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

        migrate(&mut conn).expect("v4 → current");

        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
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

    /// The v6 rule that must never take a wallet offline: a pre-existing name
    /// that resolves to a skeleton an older merchant already claims leaves this
    /// row unnamed and logs it, rather than failing the migration at startup.
    #[test]
    fn a_pre_v6_name_collision_leaves_a_null_skeleton_instead_of_failing_startup() {
        let mut conn = v4_db();
        insert_account(&conn, "acct-shop");
        for (id, name, created) in [
            ("m-first", "PiggleShop", 1),
            ("m-clash", "PiggleShoр", 2), // Cyrillic er — the same skeleton
            ("m-other", "鉱石商会", 3),
        ] {
            conn.execute(
                "INSERT INTO merchants (merchant_id, account_id, name, created_unix_ms) \
                 VALUES (?1, 'acct-shop', ?2, ?3)",
                rusqlite::params![id, name, created],
            )
            .unwrap();
        }

        migrate(&mut conn).expect("a name collision must not stop the migration");

        let skeleton = |id: &str| -> Option<String> {
            conn.query_row(
                "SELECT name_skeleton FROM merchants WHERE merchant_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        // Oldest first wins the name; the imitator keeps working but claims none.
        assert!(skeleton("m-first").is_some());
        assert_eq!(skeleton("m-clash"), None);
        assert!(skeleton("m-other").is_some());
        // And the demo merchants leave the pay tab, which is what `listed = 0`
        // defaulting is for: money paid to a handle-less account is unreachable.
        let listed: i64 = conn
            .query_row("SELECT COUNT(*) FROM merchants WHERE listed = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(listed, 0);
    }

    /// v7 must give existing sessions a NULL device link (no device suddenly
    /// starts receiving notifications it never registered for) and an outbox
    /// whose retry columns default to "due now, never tried".
    #[test]
    fn migration_v7_adds_a_null_device_link_and_a_due_now_outbox() {
        let mut conn = v4_db();
        insert_account(&conn, "acct-a");

        migrate(&mut conn).expect("v4 → current");

        conn.execute(
            "INSERT INTO moymoy_sessions \
               (session_id, account_id, token_hash, created_unix_ms, last_seen_unix_ms, expires_unix_ms) \
             VALUES ('s1', 'acct-a', 'th-1', 0, 0, 9)",
            [],
        )
        .expect("pre-v7-shaped session insert still works");
        let link: Option<String> = conn
            .query_row(
                "SELECT mochi_account_id FROM moymoy_sessions WHERE session_id = 's1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(link, None);

        conn.execute(
            "INSERT INTO notification_outbox \
               (outbox_id, account_id, kind, label, amount, created_unix_ms) \
             VALUES ('o1', 'acct-a', 'receive', 'テスト から受取', 5, 1)",
            [],
        )
        .expect("outbox insert with defaulted retry columns");
        let (attempts, due): (i64, i64) = conn
            .query_row(
                "SELECT attempts, next_attempt_unix_ms FROM notification_outbox WHERE outbox_id = 'o1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((attempts, due), (0, 0));
    }

    /// v8 rescales every amount by 100 and NOTHING else.
    ///
    /// The failure this guards against is not "the migration did not run" — it is
    /// "the migration ran over one column too few, or one too many". A missed
    /// amount column is a balance out by 100×; a scaled counter is a lockout at
    /// the wrong number of tries. So the assertions come in two halves and the
    /// control half matters as much as the other one.
    #[test]
    fn migration_v8_scales_amounts_and_leaves_counts_alone() {
        let mut conn = v7_db();
        insert_account(&conn, "acct-a");
        conn.execute(
            "UPDATE accounts SET balance = 12345, failed_pin_attempts = 3 WHERE account_id = 'acct-a'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO transactions (id, account_id, kind, label, amount, balance_after, ts_unix_ms) \
             VALUES ('t1', 'acct-a', 'charge', 'インベントリのエメラルド', 12345, 12345, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO merchants (merchant_id, account_id, name, created_unix_ms, \
               daily_issue_cap, max_open_intents) \
             VALUES ('m-capped', 'acct-a', 'Piggle Shop', 1, 50000, 20), \
                    ('m-default', 'acct-a', '鉱石商会', 2, NULL, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO payment_intents (intent_id, merchant_id, amount, description, state, \
               idem_key, created_unix_ms, updated_unix_ms, expires_unix_ms) \
             VALUES ('pi_1', 'm-capped', 129, 'りんご 1個', 'created', 'ord-1', 1, 1, 9)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO notification_outbox (outbox_id, account_id, kind, label, amount, \
               created_unix_ms, attempts) \
             VALUES ('o1', 'acct-a', 'receive', 'テスト から受取', 5, 1, 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO emerald_ops (op_id, idem_key, account_id, mc_uuid, direction, \
               requested_amount, settled_amount, state, created_unix_ms, updated_unix_ms) \
             VALUES ('op-settled', 'k-1', 'acct-a', 'uuid-1', 'charge', 40, 40, 'settled', 1, 1), \
                    ('op-open', 'k-2', 'acct-a', 'uuid-1', 'withdraw', 7, NULL, 'sent', 1, 1)",
            [],
        )
        .unwrap();
        // One record of every shape the idempotency table actually holds.
        idem_put(&conn, "k-send", "send", r#"{"ok":true,"balance":18846,"tx_id":"t-1"}"#).unwrap();
        idem_put(&conn, "k-pay", "pay", r#"{"ok":true,"balance":0,"tx_id":"t-2"}"#).unwrap();
        idem_put(
            &conn,
            "ord-1",
            "mi:m-capped",
            r#"{"ok":true,"amount":129,"intent_id":"pi_1","state":"created"}"#,
        )
        .unwrap();
        idem_put(
            &conn,
            "k-c",
            "charge:acct-a",
            r#"{"ok":true,"op_id":"op-settled","state":"pending"}"#,
        )
        .unwrap();
        idem_put(
            &conn,
            "k-w",
            "withdraw:acct-a",
            r#"{"ok":true,"op_id":"op-open","state":"pending"}"#,
        )
        .unwrap();

        migrate(&mut conn).expect("v7 → v8");

        let i = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        let n = |sql: &str| -> Option<i64> { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        let s = |sql: &str| -> String { conn.query_row(sql, [], |r| r.get(0)).unwrap() };

        // Amounts: ×100, and the balance/ledger invariant survives it because both
        // sides moved together.
        assert_eq!(i("SELECT balance FROM accounts"), 1_234_500);
        assert_eq!(i("SELECT amount FROM transactions"), 1_234_500);
        assert_eq!(i("SELECT balance_after FROM transactions"), 1_234_500);
        assert_eq!(i("SELECT amount FROM payment_intents"), 12_900);
        assert_eq!(i("SELECT amount FROM notification_outbox"), 500);
        assert_eq!(
            i("SELECT daily_issue_cap FROM merchants WHERE merchant_id = 'm-capped'"),
            5_000_000
        );
        // An unset cap stays unset: NULL means "the code's default", which is
        // scaled in merchant.rs, and turning it into 0 would stop the shop billing.
        assert_eq!(
            n("SELECT daily_issue_cap FROM merchants WHERE merchant_id = 'm-default'"),
            None
        );
        // Both emerald_ops amount columns, including the one an eye could mistake
        // for a physical emerald count. The ÷100 to reach the mod is mc.rs's job.
        assert_eq!(
            i("SELECT requested_amount FROM emerald_ops WHERE op_id = 'op-settled'"),
            4_000
        );
        assert_eq!(
            i("SELECT settled_amount FROM emerald_ops WHERE op_id = 'op-settled'"),
            4_000
        );
        assert_eq!(
            i("SELECT requested_amount FROM emerald_ops WHERE op_id = 'op-open'"),
            700
        );
        assert_eq!(
            n("SELECT settled_amount FROM emerald_ops WHERE op_id = 'op-open'"),
            None,
            "an unsettled op was given an amount it never had"
        );

        // Controls: counts of things, not amounts of money.
        assert_eq!(i("SELECT failed_pin_attempts FROM accounts"), 3);
        assert_eq!(i("SELECT attempts FROM notification_outbox"), 2);
        assert_eq!(
            i("SELECT max_open_intents FROM merchants WHERE merchant_id = 'm-capped'"),
            20
        );

        // The frozen replies. Rewritten in place — a deleted record would make the
        // client's retry re-execute, which for a send is paying twice.
        assert_eq!(
            i("SELECT json_extract(response_json,'$.balance') FROM idempotency WHERE scope = 'send'"),
            1_884_600
        );
        assert_eq!(
            i("SELECT json_extract(response_json,'$.balance') FROM idempotency WHERE scope = 'pay'"),
            0
        );
        assert_eq!(
            s("SELECT json_extract(response_json,'$.tx_id') FROM idempotency WHERE scope = 'send'"),
            "t-1",
            "the rest of a frozen reply was disturbed"
        );
        // The merchant reply is scaled AND renamed, because /merchant/v1 no longer
        // has an `amount`: a replay still answering the old key would hand the one
        // third party involved a number in a unit the live API rejects.
        assert_eq!(
            i("SELECT json_extract(response_json,'$.amount_minor') FROM idempotency WHERE scope = 'mi:m-capped'"),
            12_900
        );
        assert_eq!(
            n("SELECT json_extract(response_json,'$.amount') FROM idempotency WHERE scope = 'mi:m-capped'"),
            None,
            "the old key survived alongside the new one"
        );
        assert_eq!(
            s("SELECT json_extract(response_json,'$.intent_id') FROM idempotency WHERE scope = 'mi:m-capped'"),
            "pi_1"
        );
        // Charge and withdraw records carry no amount, so they are left exactly as
        // they were rather than rewritten for the sake of uniformity — an op_id is
        // the one thing in there, and it is what a retry is answered with.
        for (key, scope, op) in [
            ("k-c", "charge:acct-a", "op-settled"),
            ("k-w", "withdraw:acct-a", "op-open"),
        ] {
            let stored = idem_get(&conn, key, scope).unwrap().expect("record survives");
            assert_eq!(
                stored,
                format!(r#"{{"ok":true,"op_id":"{op}","state":"pending"}}"#),
                "{scope} was rewritten"
            );
        }
    }

    /// v9 must close the books on every payment that predates escrow.
    ///
    /// **This is the statement that would cost real money if it were missing.**
    /// Every `paid` intent that exists at migration time was settled straight to
    /// the merchant, so its money is already there. A NULL `released_unix_ms`
    /// makes such a row match the release sweep's filter, and the first sweep
    /// after deployment would pay it AGAIN — out of an escrow account that never
    /// received anything for it, i.e. out of other customers' held payments.
    #[test]
    fn migration_v9_closes_the_books_on_payments_that_predate_escrow() {
        let mut conn = v8_db();
        insert_account(&conn, "acct-a");
        conn.execute(
            "INSERT INTO merchants (merchant_id, account_id, name, created_unix_ms) \
             VALUES ('m1', 'acct-a', '鉱石商会', 1)",
            [],
        )
        .unwrap();
        for (id, state) in [
            ("pi_paid", "paid"),
            ("pi_paid2", "paid"),
            ("pi_open", "created"),
            ("pi_declined", "declined"),
            ("pi_expired", "expired"),
        ] {
            conn.execute(
                "INSERT INTO payment_intents (intent_id, merchant_id, amount, description, state, \
                   idem_key, created_unix_ms, updated_unix_ms, expires_unix_ms) \
                 VALUES (?1, 'm1', 12900, 'りんご 1個', ?2, ?1, 1, 1, 99999999999999)",
                rusqlite::params![id, state],
            )
            .unwrap();
        }

        migrate(&mut conn).expect("v8 → v9");
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );

        let released = |id: &str| -> Option<i64> {
            conn.query_row(
                "SELECT released_unix_ms FROM payment_intents WHERE intent_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        // Both paid rows are stamped, so the sweep cannot see them.
        for id in ["pi_paid", "pi_paid2"] {
            assert!(released(id).is_some(), "{id} was left claimable by the release sweep");
        }
        // …and nothing that was never paid is pretended to have been released.
        for id in ["pi_open", "pi_declined", "pi_expired"] {
            assert_eq!(released(id), None, "{id} was marked released");
        }

        // The stamped rows carry no `escrowed_unix_ms`, which is what tells
        // `force_refund` to take their refund from the merchant (where the money
        // actually is) rather than from escrow (which never held it).
        let escrowed: Option<i64> = conn
            .query_row(
                "SELECT escrowed_unix_ms FROM payment_intents WHERE intent_id = 'pi_paid'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(escrowed, None);

        // Every other new column starts empty on every row: nothing is fulfilled,
        // nothing is due, and no ledger rows are claimed to exist.
        let unset: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM payment_intents \
                 WHERE release_due_unix_ms IS NULL AND escrow_deadline_unix_ms IS NULL \
                   AND fulfilled_unix_ms IS NULL AND fulfilled_amount IS NULL \
                   AND release_tx_id IS NULL AND escrow_refund_tx_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unset, 5);

        // The backfill's `IS NULL` guard, which exists for a hand re-run during
        // incident recovery rather than for `migrate` (that runs the file once).
        // Re-stamping would misreport when an already-closed payment moved.
        let before = released("pi_paid");
        conn.execute(
            "UPDATE payment_intents \
                SET released_unix_ms = CAST(strftime('%s','now') AS INTEGER) * 1000 + 5000 \
              WHERE state = 'paid' AND released_unix_ms IS NULL",
            [],
        )
        .unwrap();
        assert_eq!(released("pi_paid"), before);
    }

    /// v10 adds a column and touches nothing else.
    ///
    /// A pure `ADD COLUMN` has one way to go wrong that matters here: arriving
    /// with a default that claims something. `fulfil_reason` starts NULL on every
    /// existing row, which is the truth — those reports were made before there was
    /// anywhere to put an explanation, and inventing `''` would make "gave no
    /// reason" and "gave an empty one" the same.
    #[test]
    fn migration_v10_adds_the_reason_column_without_disturbing_anything() {
        let mut conn = v8_db();
        insert_account(&conn, "acct-a");
        conn.execute(
            "INSERT INTO merchants (merchant_id, account_id, name, created_unix_ms) \
             VALUES ('m1', 'acct-a', '鉱石商会', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO payment_intents (intent_id, merchant_id, amount, description, state, \
               idem_key, created_unix_ms, updated_unix_ms, expires_unix_ms) \
             VALUES ('pi_old', 'm1', 12900, 'りんご 1個', 'paid', 'ord-1', 1, 1, 9)",
            [],
        )
        .unwrap();

        migrate(&mut conn).expect("v8 → v10");
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );

        let (reason, amount, released): (Option<String>, i64, Option<i64>) = conn
            .query_row(
                "SELECT fulfil_reason, amount, released_unix_ms FROM payment_intents \
                 WHERE intent_id = 'pi_old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(reason, None, "an explanation was invented for an old report");
        // v9's work is still intact after v10 ran on top of it.
        assert_eq!(amount, 12900);
        assert!(released.is_some(), "v9's backfill was undone");
    }

    /// v11 gives every existing account a NULL verification time.
    ///
    /// NULL is the only honest default: nobody has cleared a second factor under
    /// a mechanism that did not exist. A timestamp — even the migration's own —
    /// would silently forgive whatever those accounts had already spent, which is
    /// the reverse of what the column is for.
    #[test]
    fn migration_v11_starts_every_account_unverified() {
        let mut conn = v8_db();
        insert_account(&conn, "acct-a");
        conn.execute(
            "UPDATE accounts SET balance = 5000 WHERE account_id = 'acct-a'",
            [],
        )
        .unwrap();

        migrate(&mut conn).expect("v8 → v11");
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );

        let (verified, balance): (Option<i64>, i64) = conn
            .query_row(
                "SELECT stepup_verified_unix_ms, balance FROM accounts WHERE account_id = 'acct-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(verified, None, "an account was credited with a verification it never made");
        // The v8 rescale is still intact after three more migrations ran on it.
        assert_eq!(balance, 5000);
    }

    /// v12 adds the park column and REBUILDS the sweep's partial index around it.
    ///
    /// The index is the half that would fail silently. A partial index's predicate
    /// cannot be altered, so it has to be dropped and recreated; if that were
    /// missed, everything would still work and the sweep would simply re-scan
    /// every parked row forever.
    #[test]
    fn migration_v12_parks_by_column_and_narrows_the_sweep_index() {
        let mut conn = v8_db();
        insert_account(&conn, "acct-a");
        conn.execute(
            "INSERT INTO merchants (merchant_id, account_id, name, created_unix_ms) \
             VALUES ('m1', 'acct-a', '鉱石商会', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO payment_intents (intent_id, merchant_id, amount, description, state, \
               idem_key, created_unix_ms, updated_unix_ms, expires_unix_ms) \
             VALUES ('pi_old', 'm1', 12900, 'りんご 1個', 'paid', 'ord-1', 1, 1, 9)",
            [],
        )
        .unwrap();

        migrate(&mut conn).expect("v8 → v12");
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );

        // Nothing is parked by the migration: no deadline has run out on a payment
        // that predates the mechanism.
        let parked: Option<i64> = conn
            .query_row(
                "SELECT escrow_parked_unix_ms FROM payment_intents WHERE intent_id = 'pi_old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(parked, None);

        // The index exists exactly once and excludes parked rows.
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' \
                   AND name = 'idx_intents_release_due'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("escrow_parked_unix_ms IS NULL"),
            "the sweep index still admits parked rows: {sql}"
        );
        let copies: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' \
                   AND name = 'idx_intents_release_due'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(copies, 1);

        // v9's backfill survived three migrations landing on top of it.
        let released: Option<i64> = conn
            .query_row(
                "SELECT released_unix_ms FROM payment_intents WHERE intent_id = 'pi_old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(released.is_some(), "v9's backfill was undone");
    }

    #[test]
    fn a_future_schema_is_refused_rather_than_downgraded() {
        let mut conn = v4_db();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert!(migrate(&mut conn).is_err());
    }
}
