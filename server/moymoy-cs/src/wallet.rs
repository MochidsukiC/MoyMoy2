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

/// Largest single withdrawal accepted: 20,736 エメ = 2,304 emerald blocks = one
/// full inventory of blocks (36 slots × 64).
///
/// Far below [`MAX_AMOUNT`] on purpose, and it is not a wallet concern but an
/// in-world one: a withdrawal ends with the mod materialising items for a player,
/// so an unbounded one would ask it to generate millions of stacks and stall the
/// Minecraft server (and leave the drop on the ground). Bounding it here means a
/// large payout is several ops the player pulls at their own pace, each of which
/// settles independently.
pub const MAX_WITHDRAW_PER_OP: i64 = 20_736;

/// Label on the credit that gives a withdrawal's reserve back. Deliberately a
/// `withdraw` txn like the debit it undoes, so the withdraw filter shows the pair
/// together — a refund hidden under `charge` would read as income.
const WITHDRAW_REFUND_LABEL: &str = "出金の取消（返金）";

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
    pub kind: String, // pay | send | receive | charge | withdraw
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

/// A "send" target (recent counterparty / contact). `handle` is the MoyMoy ID
/// the app sends to (`@handle`); `id` is the backing account_id.
#[derive(Debug, Serialize)]
pub struct Friend {
    pub id: String,
    pub name: String,
    pub sub: String,
    pub handle: String,
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

/// Outcome of reserving eme for a withdrawal. Like [`TxResult`], only `Ok` is a
/// success and the rest are ordinary domain results.
///
/// There is no counterparty: the eme leaves the wallet system entirely (it comes
/// back as emeralds in a chest), so the debit stands alone.
#[derive(Debug)]
pub enum WithdrawReserve {
    Ok { tx_id: String, balance_after: i64 },
    BadAmount,
    Insufficient { balance: i64 },
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

/// Home aggregate for an existing (authenticated) account. Returns `None` if the
/// account row is gone (the caller treats that as an internal inconsistency,
/// since a valid session always points at a live account).
pub fn home(conn: &Connection, account_id: &str) -> rusqlite::Result<Option<HomeView>> {
    let acct = match identity::get(conn, account_id)? {
        Some(a) => a,
        None => return Ok(None),
    };
    let txns = history(conn, account_id, 6, "all")?;
    Ok(Some(HomeView {
        balance: acct.balance,
        profile: profile_of(&acct),
        txns,
    }))
}

/// Recent ledger rows, newest first. `filter` ∈ all|pay|send|charge|withdraw
/// (anything else ⇒ all). `receive` rows appear only under `all`.
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
        "pay" | "send" | "charge" | "withdraw" => {
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

/// Atomic transfer of `amount` エメ from `from_id` to `to_id`, inside the
/// caller's transaction. Records a debit on the sender (`kind`: send|pay,
/// `sender_label`) and a `receive` credit on the recipient. Both accounts must
/// already exist (a missing side ⇒ `UnknownTarget`) — accounts are created only
/// via `/auth/register`, never implicitly by a transfer.
///
/// The caller owns the `BEGIN IMMEDIATE` transaction so the idempotency
/// check-reserve-execute-record is one atomic unit (no TOCTOU double-spend
/// between concurrent retries of the same idem_key). This function never begins
/// or commits — it only reads/writes through `tx`.
pub fn transfer(
    tx: &rusqlite::Transaction<'_>,
    from_id: &str,
    to_id: &str,
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

    let sender = match identity::get(tx, from_id)? {
        Some(a) => a,
        None => return Ok(TxResult::UnknownTarget),
    };
    let receiver = match identity::get(tx, to_id)? {
        Some(a) => a,
        None => return Ok(TxResult::UnknownTarget),
    };

    if sender.balance < amount {
        return Ok(TxResult::Insufficient {
            balance: sender.balance,
        });
    }

    let now = now_ms();
    let sender_after = sender.balance - amount; // non-negative: checked balance >= amount
    let receiver_after = match receiver.balance.checked_add(amount) {
        Some(v) => v,
        // Practically unreachable (MAX_AMOUNT bounds growth) but honest-fail on
        // money arithmetic rather than panic/wrap.
        None => return Ok(TxResult::BadAmount),
    };

    tx.execute(
        "UPDATE accounts SET balance = ?2, updated_unix_ms = ?3 WHERE account_id = ?1",
        params![from_id, sender_after, now],
    )?;
    tx.execute(
        "UPDATE accounts SET balance = ?2, updated_unix_ms = ?3 WHERE account_id = ?1",
        params![to_id, receiver_after, now],
    )?;

    let counterparty_name = receiver.label();
    let sender_display = sender.label();

    let sender_tx_id = Uuid::new_v4().to_string();
    insert_txn(
        tx,
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
    let receive_label = format!("{sender_display} から受取");
    insert_txn(
        tx,
        &Uuid::new_v4().to_string(),
        to_id,
        "receive",
        &receive_label,
        Some(from_id),
        Some(&sender_display),
        amount,
        receiver_after,
        now,
    )?;
    queue_deposit_notification(tx, to_id, "receive", &receive_label, amount, now)?;

    // The caller commits (after recording idempotency) so the whole unit is atomic.
    Ok(TxResult::Ok {
        tx_id: sender_tx_id,
        balance_after: sender_after,
        counterparty_name,
    })
}

/// Credit `amount` to `account_id` and record a `charge` txn with `label`, inside
/// the caller's transaction. Used by the emerald-charge settlement (charge.rs,
/// label "インベントリのエメラルド") and dev funding (a distinct label, so dev eme
/// is auditable as such). Returns the new balance, or an out-of-range error on
/// overflow (honest-fail on money arithmetic).
pub fn credit_charge(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    amount: i64,
    now: i64,
    label: &str,
) -> rusqlite::Result<i64> {
    credit(tx, account_id, amount, now, "charge", label)
}

/// Give a withdrawal's reserve back, inside the caller's transaction.
///
/// Recorded as a `withdraw` txn like the debit it undoes, NOT as a `charge`: the
/// two belong to the same operation and the withdraw filter must show them as the
/// pair they are. Only [`crate::charge`] calls this, and only from the one
/// transaction that moved the op out of a non-terminal state — see the refund
/// rules there, which are what keeps a retried/duplicated failure from refunding
/// twice.
pub fn refund_withdraw(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    amount: i64,
    now: i64,
) -> rusqlite::Result<i64> {
    credit(tx, account_id, amount, now, "withdraw", WITHDRAW_REFUND_LABEL)
}

/// Credit `amount` to `account_id` and record a `kind`/`label` txn, inside the
/// caller's transaction. Returns the new balance, or an out-of-range error on
/// overflow (honest-fail on money arithmetic).
fn credit(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    amount: i64,
    now: i64,
    kind: &str,
    label: &str,
) -> rusqlite::Result<i64> {
    // Defense-in-depth: mirror the bounds check in `transfer` so future call
    // sites can't accidentally credit zero or a pathological amount.
    if amount <= 0 || amount > MAX_AMOUNT {
        return Err(rusqlite::Error::IntegralValueOutOfRange(0, amount));
    }
    let bal: i64 = tx.query_row(
        "SELECT balance FROM accounts WHERE account_id = ?1",
        [account_id],
        |r| r.get(0),
    )?;
    let after = bal
        .checked_add(amount)
        .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, amount))?;
    tx.execute(
        "UPDATE accounts SET balance = ?2, updated_unix_ms = ?3 WHERE account_id = ?1",
        params![account_id, after, now],
    )?;
    insert_txn(
        tx,
        &Uuid::new_v4().to_string(),
        account_id,
        kind,
        label,
        None,
        None,
        amount,
        after,
        now,
    )?;
    queue_deposit_notification(tx, account_id, kind, label, amount, now)?;
    Ok(after)
}

/// Queue a deposit notification for `account_id`, inside the same transaction
/// that credits it.
///
/// Every balance increase flows through [`transfer`]'s receiver side or
/// [`credit`], so these two call sites are the single choke point: a
/// notification row exists exactly when its credit committed, never otherwise
/// (a rolled-back operation leaves nothing to deliver). That property — not the
/// table itself — is why this lives here and not in the HTTP handlers, several
/// of which never see a deposit (charge settles and withdraw refunds land on
/// background settlers, and the admin refund runs in a separate CLI process).
/// Delivery, device links and retry policy are all [`crate::notify`]'s problem.
fn queue_deposit_notification(
    conn: &Connection,
    account_id: &str,
    kind: &str,
    label: &str,
    amount: i64,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO notification_outbox \
           (outbox_id, account_id, kind, label, amount, created_unix_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            Uuid::new_v4().to_string(),
            account_id,
            kind,
            label,
            amount,
            now
        ],
    )?;
    Ok(())
}

/// Debit `amount` エメ for a withdrawal, inside the caller's transaction —
/// the mirror image of [`credit_charge`], and the FIRST half of a withdrawal.
///
/// Reserve-first is not a style choice: granting emeralds before the debit would
/// mean a failed debit leaves emeralds that no eme paid for (in-world inflation),
/// while a debit whose grant fails is merely eme parked in an op, refundable by
/// [`refund_withdraw`]. So the balance moves here, before the mod is asked for
/// anything, and stays moved until the op reaches a terminal state.
///
/// `accounts.balance` carries `CHECK (balance >= 0)`, so the balance is read and
/// checked before the UPDATE (as in [`transfer`]) rather than letting a
/// constraint violation stand in for the insufficient-funds answer.
pub fn reserve_withdraw(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    amount: i64,
    now: i64,
    label: &str,
) -> rusqlite::Result<WithdrawReserve> {
    if amount <= 0 || amount > MAX_WITHDRAW_PER_OP {
        return Ok(WithdrawReserve::BadAmount);
    }
    // A missing row is an internal inconsistency (the account is the session's,
    // and sessions point at live accounts), so it errors rather than passing for
    // a zero balance.
    let bal: i64 = tx.query_row(
        "SELECT balance FROM accounts WHERE account_id = ?1",
        [account_id],
        |r| r.get(0),
    )?;
    if bal < amount {
        return Ok(WithdrawReserve::Insufficient { balance: bal });
    }
    let after = bal - amount; // non-negative: checked bal >= amount
    tx.execute(
        "UPDATE accounts SET balance = ?2, updated_unix_ms = ?3 WHERE account_id = ?1",
        params![account_id, after, now],
    )?;
    let tx_id = Uuid::new_v4().to_string();
    insert_txn(
        tx, &tx_id, account_id, "withdraw", label, None, None, -amount, after, now,
    )?;
    Ok(WithdrawReserve::Ok {
        tx_id,
        balance_after: after,
    })
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

/// Recent distinct counterparties that are real MoyMoy users (have a handle), as
/// "send" targets (most recent first). Merchants (no handle) are excluded — they
/// live in the pay tab.
pub fn friends(conn: &Connection, account_id: &str) -> rusqlite::Result<Vec<Friend>> {
    let mut stmt = conn.prepare(
        "SELECT a.account_id, a.handle, a.display_name, MAX(t.ts_unix_ms) AS last_ts \
         FROM transactions t JOIN accounts a ON a.account_id = t.counterparty_id \
         WHERE t.account_id = ?1 AND t.counterparty_id IS NOT NULL \
           AND t.kind IN ('send','pay','receive') AND a.handle IS NOT NULL \
         GROUP BY a.account_id ORDER BY last_ts DESC LIMIT 20",
    )?;
    let rows = stmt
        .query_map([account_id], |row| {
            let id: String = row.get(0)?;
            let handle: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            let display: Option<String> = row.get(2)?;
            Ok(Friend {
                id,
                name: display
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("@{handle}")),
                sub: format!("@{handle}"),
                handle,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Registered shops the wallet offers in its "pay" tab.
///
/// `listed = 1` is the whole filter and it is load-bearing: registration is
/// self-serve since v6, and `listed` defaults to 0. Without this clause every
/// shop would appear in every user's wallet the moment it registered, and the
/// default would prevent nothing.
pub fn merchants(conn: &Connection) -> rusqlite::Result<Vec<Merchant>> {
    let mut stmt = conn.prepare(
        "SELECT merchant_id, name, sub, glyph, pal FROM merchants \
         WHERE listed = 1 AND status = 'active' ORDER BY created_unix_ms ASC",
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

// `merchant_account()` lived here and resolved a merchant_id to its receiving
// account WITHOUT looking at its status, which meant a frozen shop kept
// collecting on anything already in flight. It had exactly one caller,
// `/wallet/pay`, and both are gone: a payment now resolves its merchant through
// `merchant::get`, and `payments::approve` refuses a merchant that is not active.

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
        // Merchant accounts are non-login (handle / pin_hash stay NULL); they
        // receive `pay` transfers and display by `display_name`.
        tx.execute(
            "INSERT OR IGNORE INTO accounts \
               (account_id, display_name, balance, holder, card_number, is_merchant, created_unix_ms, updated_unix_ms) \
             VALUES (?1, ?2, 0, ?3, ?4, 1, ?5, ?5)",
            // card_expiry omitted — the schema DEFAULT '07/29' is the single source of truth.
            params![
                account_id,
                name,
                name.to_uppercase(),
                identity::card_number_for(&account_id),
                now
            ],
        )?;
        // The skeleton is claimed here too, not only by the v6 backfill: without
        // it a fresh DB would let somebody register a shop under a demo name
        // while a migrated one refused the same registration.
        tx.execute(
            "INSERT INTO merchants \
               (merchant_id, account_id, name, name_skeleton, sub, glyph, pal, \
                created_unix_ms, updated_unix_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                mid,
                account_id,
                name,
                crate::merchant::name_skeleton(name),
                sub,
                glyph,
                pal,
                now
            ],
        )?;
    }
    tx.commit()?;
    tracing::info!("seeded {} demo merchants", demo.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::PooledConn;

    /// One account holding `balance` エメ, on its own in-memory DB. The pooled
    /// handle keeps the pool (and therefore the `:memory:` database) alive.
    fn account_with(balance: i64) -> PooledConn {
        let pool = crate::db::open_memory().expect("in-memory pool");
        let conn = pool.get().expect("checkout");
        conn.execute(
            "INSERT INTO accounts (account_id, balance, created_unix_ms, updated_unix_ms) \
             VALUES ('acct-a', ?1, 0, 0)",
            [balance],
        )
        .expect("seed account");
        conn
    }

    fn balance_of(conn: &Connection) -> i64 {
        balance(conn, "acct-a").expect("balance reads")
    }

    /// Every ledger row for the account as `(kind, amount, balance_after)`,
    /// oldest first.
    fn rows(conn: &Connection) -> Vec<(String, i64, i64)> {
        let mut stmt = conn
            .prepare(
                "SELECT kind, amount, balance_after FROM transactions \
                 WHERE account_id = 'acct-a' ORDER BY rowid ASC",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn a_short_balance_reserves_nothing_at_all() {
        // The one that must never half-happen: an insufficient withdrawal leaves
        // the balance and the ledger exactly as they were, so the caller can
        // abandon the request without anything to undo.
        let mut conn = account_with(100);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let r = reserve_withdraw(&tx, "acct-a", 101, 1_000, "エメラルドで受け取り").unwrap();
        assert!(
            matches!(r, WithdrawReserve::Insufficient { balance: 100 }),
            "{r:?}"
        );
        tx.commit().unwrap();
        assert_eq!(balance_of(&conn), 100);
        assert!(rows(&conn).is_empty());
    }

    #[test]
    fn a_reserve_debits_once_and_records_one_negative_withdraw_row() {
        let mut conn = account_with(100);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let r = reserve_withdraw(&tx, "acct-a", 40, 1_000, "エメラルドで受け取り").unwrap();
        let WithdrawReserve::Ok { balance_after, .. } = r else {
            panic!("expected a reserve, got {r:?}");
        };
        assert_eq!(balance_after, 60);
        tx.commit().unwrap();

        assert_eq!(balance_of(&conn), 60);
        assert_eq!(rows(&conn), vec![("withdraw".to_string(), -40, 60)]);
    }

    #[test]
    fn a_reserve_is_bounded_by_what_the_mod_can_materialise() {
        let mut conn = account_with(MAX_WITHDRAW_PER_OP * 2);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        for bad in [0, -1, MAX_WITHDRAW_PER_OP + 1] {
            let r = reserve_withdraw(&tx, "acct-a", bad, 1_000, "エメラルドで受け取り").unwrap();
            assert!(matches!(r, WithdrawReserve::BadAmount), "{bad} was reserved");
        }
        // …and the bound itself is reachable (funds permitting), so the limit is
        // the stated one and not one-off.
        let r = reserve_withdraw(
            &tx,
            "acct-a",
            MAX_WITHDRAW_PER_OP,
            1_000,
            "エメラルドで受け取り",
        )
        .unwrap();
        assert!(matches!(r, WithdrawReserve::Ok { .. }), "{r:?}");
        tx.commit().unwrap();
        assert_eq!(balance_of(&conn), MAX_WITHDRAW_PER_OP);
        assert_eq!(rows(&conn).len(), 1);
    }

    #[test]
    fn a_refund_is_a_withdraw_row_so_the_pair_reads_as_one_operation() {
        // A refund under `charge` would show up in the charge filter as income
        // the player never charged; under `withdraw` it sits next to the debit it
        // undoes.
        let mut conn = account_with(100);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        reserve_withdraw(&tx, "acct-a", 40, 1_000, "エメラルドで受け取り").unwrap();
        let after = refund_withdraw(&tx, "acct-a", 40, 2_000).unwrap();
        tx.commit().unwrap();

        assert_eq!(after, 100);
        assert_eq!(balance_of(&conn), 100);
        assert_eq!(
            rows(&conn),
            vec![
                ("withdraw".to_string(), -40, 60),
                ("withdraw".to_string(), 40, 100),
            ]
        );
        // Both rows are in the withdraw filter, and neither leaks into charge.
        assert_eq!(history(&conn, "acct-a", 50, "withdraw").unwrap().len(), 2);
        assert!(history(&conn, "acct-a", 50, "charge").unwrap().is_empty());
    }

    /// Every outbox row as `(account_id, kind, label, amount)`, oldest first.
    fn outbox(conn: &Connection) -> Vec<(String, String, String, i64)> {
        let mut stmt = conn
            .prepare(
                "SELECT account_id, kind, label, amount FROM notification_outbox \
                 ORDER BY rowid ASC",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn a_transfer_queues_one_notification_for_the_receiver_only() {
        let mut conn = account_with(100);
        conn.execute(
            "INSERT INTO accounts (account_id, handle, handle_lower, display_name, balance, \
               created_unix_ms, updated_unix_ms) \
             VALUES ('acct-b', 'bob', 'bob', 'Bob', 0, 0, 0)",
            [],
        )
        .unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let r = transfer(&tx, "acct-a", "acct-b", 40, "send", "@bob へ送金").unwrap();
        assert!(matches!(r, TxResult::Ok { .. }), "{r:?}");
        tx.commit().unwrap();

        // One row, for the credited side, carrying the receiver-facing label —
        // the debited sender gets nothing (a deposit notice, not an activity log).
        let queued = outbox(&conn);
        assert_eq!(queued.len(), 1);
        let (account_id, kind, label, amount) = &queued[0];
        assert_eq!((account_id.as_str(), kind.as_str(), *amount), ("acct-b", "receive", 40));
        let receiver_label: String = conn
            .query_row(
                "SELECT label FROM transactions WHERE account_id = 'acct-b' AND kind = 'receive'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(label, &receiver_label);
    }

    #[test]
    fn a_credit_queues_one_notification_with_its_kind_and_label() {
        let mut conn = account_with(0);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        credit_charge(&tx, "acct-a", 27, 1_000, "インベントリのエメラルド").unwrap();
        tx.commit().unwrap();
        assert_eq!(
            outbox(&conn),
            vec![(
                "acct-a".to_string(),
                "charge".to_string(),
                "インベントリのエメラルド".to_string(),
                27
            )]
        );
    }

    #[test]
    fn a_rolled_back_credit_leaves_no_notification_behind() {
        // The property the outbox exists for: no commit, no notification. A
        // delivery loop that ran right now must find nothing to say.
        let mut conn = account_with(0);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        credit_charge(&tx, "acct-a", 27, 1_000, "インベントリのエメラルド").unwrap();
        drop(tx); // rollback
        assert!(outbox(&conn).is_empty());
        assert_eq!(balance_of(&conn), 0);
    }

    #[test]
    fn a_withdraw_reserve_is_a_debit_and_queues_nothing() {
        let mut conn = account_with(100);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        reserve_withdraw(&tx, "acct-a", 40, 1_000, "エメラルドで受け取り").unwrap();
        tx.commit().unwrap();
        assert!(outbox(&conn).is_empty());
    }
}
