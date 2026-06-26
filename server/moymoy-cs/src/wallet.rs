//! Wallet domain: balance, history, transfers (send / pay), friends, merchants,
//! and the home aggregate. All synchronous rusqlite — invoked from async
//! handlers via `spawn_blocking`. Balance moves run inside a single
//! `BEGIN IMMEDIATE` transaction (read → check → debit → credit → ledger).

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use uuid::Uuid;

use crate::db::now_ms;
use crate::identity::{self, Account};

/// Largest single transfer/charge accepted (defensive bound).
pub const MAX_AMOUNT: i64 = 1_000_000_000;

/// Cosmetic card face (design: holder / number / expiry).
#[derive(Debug, Serialize)]
pub struct Profile {
    pub holder: String,
    pub number: String,
    pub expiry: String,
}

/// One ledger row as the app consumes it. `ts` is epoch ms; the client formats
/// the "今日 14:22" label.
#[derive(Debug, Serialize)]
pub struct Txn {
    pub id: String,
    pub kind: String, // pay | send | receive | charge
    pub label: String,
    pub amount: i64, // signed (this account's perspective)
    pub ts: i64,
}

/// Home-screen aggregate (balance + card + recent activity).
#[derive(Debug, Serialize)]
pub struct HomeView {
    pub balance: i64,
    pub profile: Profile,
    pub txns: Vec<Txn>,
}

/// A "send" target (recent counterparty / contact).
#[derive(Debug, Serialize)]
pub struct Friend {
    pub id: String,
    pub name: String,
    pub sub: String,
}

/// A "pay" target (registered shop).
#[derive(Debug, Serialize)]
pub struct Merchant {
    pub id: String,
    pub name: String,
    pub sub: Option<String>,
    pub glyph: Option<String>,
    pub pal: Option<String>,
}

/// Outcome of a balance-moving operation. Only `Ok` is a success; the rest are
/// ordinary domain results (HTTP 200, `ok:false`), not faults.
#[derive(Debug)]
pub enum TxResult {
    Ok {
        tx_id: String,
        balance_after: i64,
        counterparty_name: String,
    },
    BadAmount,
    SelfTransfer,
    UnknownTarget,
    Insufficient {
        balance: i64,
    },
}

fn profile_of(a: &Account) -> Profile {
    Profile {
        holder: a.holder.clone(),
        number: a.card_number.clone(),
        expiry: a.card_expiry.clone(),
    }
}

/// Balance for an account (0 if it has never transacted; does not create a row).
pub fn balance(conn: &Connection, account_id: &str) -> rusqlite::Result<i64> {
    Ok(conn
        .query_row(
            "SELECT balance FROM accounts WHERE account_id = ?1",
            [account_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Home aggregate. Provisions the account on first sight so a new player sees a
/// zero-balance card immediately.
pub fn home(conn: &Connection, account_id: &str, mcid: Option<&str>) -> rusqlite::Result<HomeView> {
    let acct = identity::get_or_create(conn, account_id, mcid)?;
    let txns = history(conn, account_id, 6, "all")?;
    Ok(HomeView {
        balance: acct.balance,
        profile: profile_of(&acct),
        txns,
    })
}

/// Recent ledger rows, newest first. `filter` ∈ all|pay|send|charge (anything
/// else ⇒ all). `receive` rows appear only under `all`.
pub fn history(
    conn: &Connection,
    account_id: &str,
    limit: i64,
    filter: &str,
) -> rusqlite::Result<Vec<Txn>> {
    let limit = limit.clamp(1, 200);
    let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Txn> {
        Ok(Txn {
            id: row.get("id")?,
            kind: row.get("kind")?,
            label: row.get("label")?,
            amount: row.get("amount")?,
            ts: row.get("ts_unix_ms")?,
        })
    };
    let rows = match filter {
        "pay" | "send" | "charge" => {
            let mut stmt = conn.prepare(
                "SELECT id, kind, label, amount, ts_unix_ms FROM transactions \
                 WHERE account_id = ?1 AND kind = ?2 ORDER BY ts_unix_ms DESC LIMIT ?3",
            )?;
            // Bind to a local so the borrowing `MappedRows` temporary drops at the
            // `;` (before `stmt`), not at the end of the match arm.
            let v = stmt
                .query_map(params![account_id, filter, limit], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        }
        _ => {
            let mut stmt = conn.prepare(
                "SELECT id, kind, label, amount, ts_unix_ms FROM transactions \
                 WHERE account_id = ?1 ORDER BY ts_unix_ms DESC LIMIT ?2",
            )?;
            let v = stmt
                .query_map(params![account_id, limit], map)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        }
    };
    Ok(rows)
}

/// Atomic transfer of `amount` エメ from `from_id` to `to_id`. Records a debit on
/// the sender (`kind`: send|pay, `sender_label`) and a `receive` credit on the
/// recipient. The whole read-check-update-ledger runs in one `BEGIN IMMEDIATE`.
#[allow(clippy::too_many_arguments)]
pub fn transfer(
    conn: &mut Connection,
    from_id: &str,
    from_name: Option<&str>,
    to_id: &str,
    to_name: Option<&str>,
    amount: i64,
    kind: &str,
    sender_label: &str,
) -> rusqlite::Result<TxResult> {
    if amount <= 0 || amount > MAX_AMOUNT {
        return Ok(TxResult::BadAmount);
    }
    if from_id == to_id {
        return Ok(TxResult::SelfTransfer);
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let sender = identity::get_or_create(&tx, from_id, from_name)?;
    let receiver = identity::get_or_create(&tx, to_id, to_name)?;

    if sender.balance < amount {
        // Read-only path: dropping the tx rolls back the (idempotent) get_or_create
        // upserts of names — acceptable, they re-apply on the next call.
        return Ok(TxResult::Insufficient {
            balance: sender.balance,
        });
    }

    let now = now_ms();
    let sender_after = sender.balance - amount;
    let receiver_after = receiver.balance + amount;

    tx.execute(
        "UPDATE accounts SET balance = ?2, updated_unix_ms = ?3 WHERE account_id = ?1",
        params![from_id, sender_after, now],
    )?;
    tx.execute(
        "UPDATE accounts SET balance = ?2, updated_unix_ms = ?3 WHERE account_id = ?1",
        params![to_id, receiver_after, now],
    )?;

    let counterparty_name = receiver
        .mcid
        .clone()
        .or_else(|| to_name.map(str::to_string))
        .unwrap_or_else(|| sender_label.to_string());
    let sender_display = sender
        .mcid
        .clone()
        .or_else(|| from_name.map(str::to_string))
        .unwrap_or_else(|| "プレイヤー".to_string());

    let sender_tx_id = Uuid::new_v4().to_string();
    insert_txn(
        &tx,
        &sender_tx_id,
        from_id,
        kind,
        sender_label,
        Some(to_id),
        Some(&counterparty_name),
        -amount,
        sender_after,
        now,
    )?;
    insert_txn(
        &tx,
        &Uuid::new_v4().to_string(),
        to_id,
        "receive",
        &format!("{sender_display} から受取"),
        Some(from_id),
        Some(&sender_display),
        amount,
        receiver_after,
        now,
    )?;

    tx.commit()?;
    Ok(TxResult::Ok {
        tx_id: sender_tx_id,
        balance_after: sender_after,
        counterparty_name,
    })
}

/// Credit `amount` to `account_id` and record a `charge` txn, inside the caller's
/// transaction. Used by the emerald-charge settlement (charge.rs). Returns the
/// new balance.
pub fn credit_charge(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    amount: i64,
    now: i64,
) -> rusqlite::Result<i64> {
    let bal: i64 = tx.query_row(
        "SELECT balance FROM accounts WHERE account_id = ?1",
        [account_id],
        |r| r.get(0),
    )?;
    let after = bal + amount;
    tx.execute(
        "UPDATE accounts SET balance = ?2, updated_unix_ms = ?3 WHERE account_id = ?1",
        params![account_id, after, now],
    )?;
    insert_txn(
        tx,
        &Uuid::new_v4().to_string(),
        account_id,
        "charge",
        "インベントリのエメラルド",
        None,
        None,
        amount,
        after,
        now,
    )?;
    Ok(after)
}

#[allow(clippy::too_many_arguments)]
fn insert_txn(
    conn: &Connection,
    id: &str,
    account_id: &str,
    kind: &str,
    label: &str,
    counterparty_id: Option<&str>,
    counterparty_name: Option<&str>,
    amount: i64,
    balance_after: i64,
    ts: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO transactions \
           (id, account_id, kind, label, counterparty_id, counterparty_name, amount, balance_after, memo, ts_unix_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9)",
        params![id, account_id, kind, label, counterparty_id, counterparty_name, amount, balance_after, ts],
    )?;
    Ok(())
}

/// Recent distinct counterparties as "send" targets (most recent first).
pub fn friends(conn: &Connection, account_id: &str) -> rusqlite::Result<Vec<Friend>> {
    let mut stmt = conn.prepare(
        "SELECT counterparty_id, counterparty_name, MAX(ts_unix_ms) AS last_ts \
         FROM transactions \
         WHERE account_id = ?1 AND counterparty_id IS NOT NULL AND kind IN ('send','pay','receive') \
         GROUP BY counterparty_id ORDER BY last_ts DESC LIMIT 20",
    )?;
    let rows = stmt
        .query_map([account_id], |row| {
            let id: String = row.get(0)?;
            let name: Option<String> = row.get(1)?;
            Ok(Friend {
                id: id.clone(),
                name: name.clone().unwrap_or_else(|| "プレイヤー".to_string()),
                sub: name
                    .map(|n| format!("@{}", n.to_lowercase()))
                    .unwrap_or_else(|| format!("@{}", &id[..id.len().min(8)])),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Registered shops (the "pay" tab list).
pub fn merchants(conn: &Connection) -> rusqlite::Result<Vec<Merchant>> {
    let mut stmt = conn.prepare(
        "SELECT merchant_id, name, sub, glyph, pal FROM merchants ORDER BY created_unix_ms ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(Merchant {
                id: row.get(0)?,
                name: row.get(1)?,
                sub: row.get(2)?,
                glyph: row.get(3)?,
                pal: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Resolve a merchant_id to its backing account_id + display name.
pub fn merchant_account(conn: &Connection, merchant_id: &str) -> rusqlite::Result<Option<(String, String)>> {
    conn.query_row(
        "SELECT account_id, name FROM merchants WHERE merchant_id = ?1",
        [merchant_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .optional()
}

/// Seed the design's demo merchants (and their backing accounts) once, so the
/// "pay" tab is populated in a fresh dev DB. No-op when any merchant exists.
pub fn seed_demo_merchants(conn: &mut Connection) -> rusqlite::Result<()> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM merchants", [], |r| r.get(0))?;
    if count > 0 {
        return Ok(());
    }
    // (merchant_id, name, sub, glyph, pal) — mirrors src/moymoy-screens.jsx MOY_MERCHANTS.
    let demo = [
        ("m1", "鉱石商会", "総合ストア", "◈", "emerald"),
        ("m2", "エンダー雑貨店", "ブロック・道具", "▦", "purple"),
        ("m3", "ダイヤ鍛冶屋", "防具・武具", "⚒", "ice"),
        ("m4", "村人A の露店", "食料・農作物", "✦", "meadow"),
        ("m5", "レッドストン技研", "回路パーツ", "⚙", "red"),
    ];
    let now = now_ms();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (mid, name, sub, glyph, pal) in demo {
        let account_id = identity::offline_uuid(&format!("merchant:{mid}"))
            .hyphenated()
            .to_string();
        tx.execute(
            "INSERT OR IGNORE INTO accounts \
               (account_id, mcid, balance, holder, card_number, card_expiry, is_merchant, created_unix_ms, updated_unix_ms) \
             VALUES (?1, ?2, 0, ?3, ?4, '07/29', 1, ?5, ?5)",
            params![
                account_id,
                name,
                name.to_uppercase(),
                identity::card_number_for(&account_id),
                now
            ],
        )?;
        tx.execute(
            "INSERT INTO merchants (merchant_id, account_id, name, sub, glyph, pal, created_unix_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![mid, account_id, name, sub, glyph, pal, now],
        )?;
    }
    tx.commit()?;
    tracing::info!("seeded {} demo merchants", demo.len());
    Ok(())
}
