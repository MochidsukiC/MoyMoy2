//! Fixtures shared by the charge and withdraw test modules.
//!
//! Extracted so both can build the same ledger states without either one
//! reaching into the other's test module.

use super::*;
use crate::db::Pool;

pub(super) fn coordinator() -> (Pool, ChargeCoordinator) {
    let pool = crate::db::open_memory().expect("in-memory pool");
    // A sender that never connected: every send reports `NotSent`, so an op
    // that IS driven keeps its state and one that is escalated visibly moves.
    let mc = McLink::new(mochi_hub_cs_sdk::CsHttpSender::default());
    assert!(!mc.is_connected());
    // emerald_ops.account_id is a foreign key, and the bundled SQLite
    // enforces those by default.
    pool.get()
        .unwrap()
        .execute(
            "INSERT INTO accounts (account_id, created_unix_ms, updated_unix_ms) \
             VALUES ('acct-a', 0, 0)",
            [],
        )
        .unwrap();
    (pool.clone(), ChargeCoordinator::new(pool, mc))
}

pub(super) fn insert_op(pool: &Pool, op_id: &str, attester: Option<&str>, state: &str) {
    insert_full_op(pool, op_id, attester, "charge", state, 10, now_ms());
}

/// An `emerald_ops` row exactly as written, for the cases a test needs to
/// control the direction, the amount or the age.
pub(super) fn insert_full_op(
    pool: &Pool,
    op_id: &str,
    attester: Option<&str>,
    direction: &str,
    state: &str,
    amount: i64,
    created: i64,
) {
    let conn = pool.get().unwrap();
    conn.execute(
        "INSERT INTO emerald_ops (op_id, idem_key, account_id, mc_uuid, attester_id, direction, \
           requested_amount, settled_amount, state, created_unix_ms, updated_unix_ms) \
         VALUES (?1, ?1, 'acct-a', '11111111-2222-4333-8444-555555555555', ?2, ?3, \
                 ?4, NULL, ?5, ?6, ?6)",
        params![op_id, attester, direction, amount, state, created],
    )
    .unwrap();
}

/// A withdraw op whose eme is already reserved (the account balance the test
/// sets is what is left AFTER the debit), old enough to be dead-lettered when
/// `aged` is true.
pub(super) fn insert_withdraw(pool: &Pool, op_id: &str, state: &str, amount: i64, aged: bool) {
    let created = if aged {
        now_ms() - DEAD_LETTER_MS - 1_000
    } else {
        now_ms()
    };
    insert_full_op(pool, op_id, Some("mc1"), "withdraw", state, amount, created);
}

pub(super) fn state_of(pool: &Pool, op_id: &str) -> String {
    pool.get()
        .unwrap()
        .query_row(
            "SELECT state FROM emerald_ops WHERE op_id = ?1",
            [op_id],
            |r| r.get(0),
        )
        .unwrap()
}

pub(super) fn settled_of(pool: &Pool, op_id: &str) -> Option<i64> {
    pool.get()
        .unwrap()
        .query_row(
            "SELECT settled_amount FROM emerald_ops WHERE op_id = ?1",
            [op_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// Set the account's balance (what it holds after any reserve a test models).
pub(super) fn fund(pool: &Pool, amount: i64) {
    pool.get()
        .unwrap()
        .execute(
            "UPDATE accounts SET balance = ?1 WHERE account_id = 'acct-a'",
            [amount],
        )
        .unwrap();
}

pub(super) fn balance_of(pool: &Pool) -> i64 {
    pool.get()
        .unwrap()
        .query_row(
            "SELECT balance FROM accounts WHERE account_id = 'acct-a'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// Every ledger row as `(kind, amount)`, oldest first — a refund is a positive
/// `withdraw`, so counting these is how "refunded exactly once" is asserted.
pub(super) fn txns(pool: &Pool) -> Vec<(String, i64)> {
    let conn = pool.get().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT kind, amount FROM transactions WHERE account_id = 'acct-a' \
             ORDER BY rowid ASC",
        )
        .unwrap();
    let v = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    v
}

/// Apply a withdraw ack. Scoped so the pooled connection is returned before
/// the assertions (the in-memory pool holds exactly one).
pub(super) fn withdraw_ack(pool: &Pool, ack: Value) {
    let mut conn = pool.get().unwrap();
    settle_withdraw_ack(&mut conn, &ack).expect("the settlement applies");
}

pub(super) fn charge_ack(pool: &Pool, ack: Value) {
    let mut conn = pool.get().unwrap();
    settle_ack(&mut conn, &ack).expect("the settlement applies");
}
