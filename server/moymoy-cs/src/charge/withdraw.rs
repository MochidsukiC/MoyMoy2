//! The withdraw half of the emerald ledger: eme → in-world emeralds.
//!
//! Split out of [`super`] for size, not for independence — the two directions
//! share one coordinator, one `emerald_ops` table and one reconciliation pass,
//! and the parent's module docs are still where the model as a whole is
//! explained. What lives here is everything only a withdrawal needs: reserving
//! the eme up front, settling the mod's `granted` ack, and the refund rules that
//! keep a payout from being paid twice or given back twice.
//!
//! It is a CHILD module on purpose. `ChargeCoordinator`'s pool and link stay
//! private to `charge`, so nothing had to be opened up to the rest of the crate
//! to move this out.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::db::{self, now_ms};
use crate::error::ApiError;
use crate::wallet;

use super::{ack_amount, ChargeCoordinator, OpDirection};

/// Withdraw-txn label on the debit that reserves the payout.
const WITHDRAW_LABEL: &str = "エメラルドで受け取り";

/// Internal outcome of the begin-withdraw transaction. Unlike
/// [`BeginCharge`](super::BeginCharge) it carries the refusals too: a withdrawal
/// is refused by the same transaction that would have reserved the eme, so the
/// answer comes back from in there.
enum BeginWithdraw {
    /// A prior op exists for this idem_key — replay it.
    Existing(String),
    /// A fresh op was created and its eme is reserved.
    Fresh(String),
    /// Out of bounds (defence in depth — the caller checks first).
    BadAmount,
    /// Not enough eme. Nothing was written: no debit, no op, and no idempotency
    /// record, so the same key works again once the balance covers it.
    Insufficient(i64),
}

/// The idempotency scope for a withdrawal: `withdraw:<account_id>`.
///
/// Account-scoped for the same reason [`charge_scope`](super::charge_scope) is,
/// and a DIFFERENT prefix so one account's own key cannot cross the two:
/// replaying a charge's key against `/wallet/withdraw` must not hand back the
/// charge's op (nor stop the withdrawal being created), because the two move
/// value in opposite directions.
pub fn withdraw_scope(account_id: &str) -> String {
    format!("withdraw:{account_id}")
}

impl ChargeCoordinator {
    /// The frozen response for an existing `(account_id, idem_key)` withdrawal, if
    /// there is one — [`replay_charge`](Self::replay_charge) for the other
    /// direction, and needing no assertion for the same reason.
    ///
    /// A replay is if anything more important here: the first call already
    /// debited, so re-running it would debit twice, and asking for consent again
    /// would prompt the user to approve a payout that is already in flight.
    pub async fn replay_withdraw(
        &self,
        account_id: &str,
        idem_key: &str,
    ) -> Result<Option<Value>, ApiError> {
        self.replay_op(withdraw_scope(account_id), "withdraw_failed", idem_key)
            .await
    }

    /// Begin a withdrawal: debit `amount` (minor units), record a pending
    /// `emerald_ops` row (idempotent on `(account_id, idem_key)`), ask the mod on
    /// `attester_id` to grant the emeralds to `mc_uuid`, and return a pollable op
    /// (`GET /wallet/op`). The mirror of [`begin_charge`](Self::begin_charge),
    /// with the order of the two halves reversed — see the module docs.
    ///
    /// The debit happens whether or not the payout lands, and stays until the op
    /// reaches a terminal state: `settled` (the mod granted), or `failed` (it
    /// provably did not, which refunds in the same transaction). An op whose
    /// outcome is unknown stays open for reconciliation and ends at `stuck` rather
    /// than being refunded on a guess.
    pub async fn begin_withdraw(
        &self,
        idem_key: &str,
        account_id: &str,
        mc_uuid: &str,
        attester_id: &str,
        amount: i64,
    ) -> Result<Value, ApiError> {
        if amount <= 0 || amount > wallet::MAX_WITHDRAW_PER_OP {
            return Ok(json!({ "ok": false, "error": "bad_amount" }));
        }
        // Parsed BEFORE anything is written — the opposite order to `begin_charge`,
        // which creates the op first and fails it on a bad UUID. That is fine when
        // the failure costs nothing; here it would mean the eme was already
        // debited and owed back. So nothing moves until the destination is known
        // to be addressable.
        let uuid = match Uuid::parse_str(mc_uuid) {
            Ok(u) => u,
            Err(_) => return Ok(json!({ "ok": false, "error": "bad_uuid" })),
        };

        // 1. Reserve the eme and create the op in ONE transaction.
        let pool = self.pool.clone();
        let ik = idem_key.to_string();
        let aid = account_id.to_string();
        let scope = withdraw_scope(account_id);
        let muuid = mc_uuid.to_string();
        let att = attester_id.to_string();
        let outcome = tokio::task::spawn_blocking(move || -> Result<BeginWithdraw, ApiError> {
            let mut conn = pool.get()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(prev) = db::idem_get(&tx, &ik, &scope)? {
                let op = match serde_json::from_str::<Value>(&prev) {
                    Ok(v) => v.get("op_id").and_then(Value::as_str).unwrap_or("").to_string(),
                    Err(e) => {
                        tracing::error!(error = %e, idem_key = %ik,
                            "begin_withdraw: corrupt idempotency record; treating as withdraw_failed (not fabricating success)");
                        String::new()
                    }
                };
                return Ok(BeginWithdraw::Existing(op));
            }
            let now = now_ms();
            // The debit and the op that owes it back are one atomic unit: an op
            // with no reserve would let the mod grant emeralds nothing paid for,
            // and a reserve with no op would be eme nothing ever refunds.
            let (tx_id, balance_after) =
                match wallet::reserve_withdraw(&tx, &aid, amount, now, WITHDRAW_LABEL)? {
                    // Both refusals return without committing, so the rollback
                    // leaves no debit, no op and — deliberately — no idempotency
                    // record: the same key must work again once the balance covers
                    // it.
                    wallet::WithdrawReserve::BadAmount => return Ok(BeginWithdraw::BadAmount),
                    wallet::WithdrawReserve::Insufficient { balance } => {
                        return Ok(BeginWithdraw::Insufficient(balance))
                    }
                    wallet::WithdrawReserve::Ok {
                        tx_id,
                        balance_after,
                    } => (tx_id, balance_after),
                };
            let op_id = Uuid::new_v4().to_string();
            // attester_id is persisted for the same reason as on a charge: a retry
            // must reach the server the user consented to, not whichever one they
            // are on later.
            tx.execute(
                "INSERT INTO emerald_ops \
                   (op_id, idem_key, account_id, mc_uuid, attester_id, direction, requested_amount, settled_amount, state, created_unix_ms, updated_unix_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 'pending', ?8, ?8)",
                params![op_id, ik, aid, muuid, att, OpDirection::Withdraw.as_str(), amount, now],
            )?;
            let resp = json!({ "ok": true, "op_id": op_id, "state": "pending" });
            db::idem_put(&tx, &ik, &scope, &resp.to_string())?;
            tx.commit()?;
            // The debit's own ledger id, so the op and the row that funds it can be
            // matched up when a `stuck` one is reviewed by hand.
            tracing::info!(op_id, tx_id, amount, balance_after, "withdraw reserved");
            Ok(BeginWithdraw::Fresh(op_id))
        })
        .await??;

        let op_id = match outcome {
            BeginWithdraw::Fresh(op) => op,
            BeginWithdraw::Existing(op) if op.is_empty() => {
                return Ok(json!({ "ok": false, "error": "withdraw_failed" }))
            }
            BeginWithdraw::Existing(op) => {
                return Ok(
                    json!({ "ok": true, "op_id": op, "state": "pending", "duplicate": true }),
                )
            }
            BeginWithdraw::BadAmount => return Ok(json!({ "ok": false, "error": "bad_amount" })),
            BeginWithdraw::Insufficient(balance) => {
                return Ok(json!({ "ok": false, "error": "insufficient", "balance": balance }))
            }
        };

        // 2. Ask the mod on the consented server to grant, and settle its ack. The
        // eme is already debited, so the outcomes that report no transition leave
        // the op 'pending' for reconciliation — which re-drives it and, if it
        // never lands, dead-letters it into a refund.
        let state = self
            .drive(
                OpDirection::Withdraw,
                attester_id,
                &uuid,
                &op_id,
                idem_key,
                amount,
                "begin_withdraw",
            )
            .await?
            .unwrap_or_else(|| "pending".to_string());
        Ok(json!({ "ok": true, "op_id": op_id, "state": state }))
    }

    /// Run one withdraw settlement on the blocking pool (best-effort; failures are
    /// logged, not fatal — the op stays where it is and reconciliation retries).
    pub(super) async fn apply_withdraw(
        &self,
        op_id: &str,
        settlement: WithdrawSettlement,
        from: WithdrawFrom,
        reason: &'static str,
    ) {
        let pool = self.pool.clone();
        let op = op_id.to_string();
        let joined = tokio::task::spawn_blocking(move || match pool.get() {
            Ok(mut conn) => {
                if let Err(e) = apply_withdraw_settlement(&mut conn, &op, settlement, from, reason)
                {
                    tracing::error!(error = %e, op_id = %op, reason,
                        "withdraw settle: failed to apply; the op is unchanged and stays open");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, op_id = %op, reason, "withdraw settle: pool.get() failed");
            }
        })
        .await;
        if let Err(e) = joined {
            tracing::error!(error = %e, reason, "withdraw settle: spawn_blocking join failed (panic or shutdown)");
        }
    }
}

/// What a withdrawal's outcome turned out to be. Three values, not two, because
/// "we don't know" is a real answer here and rounding it either way loses
/// somebody's assets.
#[derive(Clone, Copy, Debug)]
pub(super) enum WithdrawSettlement {
    /// What the mod reported granting, in MINOR UNITS — the ack states a count of
    /// emeralds and `ack_amount` converts it, so this compares against
    /// `requested_amount` directly. Any shortfall against the reserve is refunded,
    /// because the ack proves it was not paid.
    Granted(i64),
    /// Nothing was granted, and that is established — the mod said so, or the op
    /// provably never reached it. The whole reserve goes back.
    NotGranted,
    /// Whether the player got the emeralds cannot be established. Nothing is
    /// refunded (that would risk paying twice); the op is parked as `stuck` for a
    /// human, exactly as an ambiguous charge is.
    Unknown,
}

/// Which states a withdraw settlement may move an op OUT of.
///
/// This is where "never refund something that may already have been granted"
/// lives. It is expressed as part of the UPDATE — never as a separate check —
/// so the row count tells the caller whether THIS transaction is the one that
/// moved the op, which is what makes the refund unrepeatable.
#[derive(Clone, Copy, Debug)]
pub(super) enum WithdrawFrom {
    /// `pending` only: the op provably never left this process, so nothing can
    /// have been granted. The refund on a dead-lettered or undeliverable op uses
    /// this — a row that has meanwhile been sent must not be refunded by it.
    NeverSent,
    /// `pending` or `sent`: open, and not yet parked for review. Everything that
    /// terminates an op on the mod's own answer uses this. `stuck` is excluded
    /// deliberately — see [`WithdrawFrom::AnyOpen`].
    NotParked,
    /// Anything non-terminal, `stuck` included. Only for a settlement that PROVES
    /// the payout landed: a late ack closing a stuck op is the correct end for it,
    /// whereas letting a failure ack refund one would hand the player both the
    /// emeralds and the eme.
    AnyOpen,
}

impl WithdrawFrom {
    /// The state predicate, as a compile-time constant (nothing user-supplied
    /// reaches the SQL below).
    fn sql(self) -> &'static str {
        match self {
            WithdrawFrom::NeverSent => "state = 'pending'",
            WithdrawFrom::NotParked => "state IN ('pending','sent')",
            WithdrawFrom::AnyOpen => "state NOT IN ('settled','failed')",
        }
    }
}

/// Settle a withdraw ack into the ledger. `ack` = `{op_id, status, granted}`.
///
/// `granted`, not `settled`: the field names are deliberately disjoint so a charge
/// ack cannot be misread as a payout (or the reverse) if one is ever routed to
/// the wrong settler.
///
/// | status | meaning | ledger | refund |
/// |---|---|---|---|
/// | `ok`/`duplicate`, `granted` ≥ requested | paid in full | `settled` | none |
/// | `ok`/`duplicate`, 0 < `granted` < requested | partial (shouldn't happen) | `settled` | the shortfall |
/// | `ok`/`duplicate`, `granted` ≤ 0 | paid nothing | `failed` | all |
/// | `player_offline`/`bad_request`/`unauthorized` | refused, nothing paid | `failed` | all |
/// | `unknown`/`internal_error`/anything else | payout unprovable | `stuck` | none |
pub fn settle_withdraw_ack(conn: &mut Connection, ack: &Value) -> rusqlite::Result<()> {
    let op_id = ack.get("op_id").and_then(Value::as_str).unwrap_or("");
    if op_id.is_empty() {
        tracing::warn!(ack = %ack, "withdraw settle: ack with missing/empty op_id (dropping)");
        return Ok(());
    }
    let status = ack.get("status").and_then(Value::as_str).unwrap_or("");
    let granted = ack_amount(ack, "granted");

    let (settlement, from, reason) = match status {
        "ok" | "duplicate" => match granted {
            // A stated amount is proof of what was paid, so it may close even a
            // stuck op — and a stated shortfall is proof the rest was not paid.
            Some(n) if n > 0 => (WithdrawSettlement::Granted(n), WithdrawFrom::AnyOpen, "ok"),
            // The mod actively reported paying nothing. Refundable.
            Some(_) => (
                WithdrawSettlement::NotGranted,
                WithdrawFrom::NotParked,
                "granted_zero",
            ),
            // "ok" with no readable amount: the mod says it paid, but not how
            // much. Treating that as 0 (the charge settler's default) would refund
            // a payout the player may be holding — the one outcome this module
            // exists to prevent. It is an unknown, and it is recorded as one.
            None => {
                tracing::error!(op_id, status, ack = %ack,
                    "withdraw ack claims success but carries no readable 'granted' — payout amount is \
                     UNKNOWN; parking as 'stuck' rather than refunding (the mod may have paid)");
                (
                    WithdrawSettlement::Unknown,
                    WithdrawFrom::NotParked,
                    "granted_unreadable",
                )
            }
        },
        // Refusals the mod decides BEFORE granting anything.
        "player_offline" | "bad_request" | "unauthorized" => (
            WithdrawSettlement::NotGranted,
            WithdrawFrom::NotParked,
            "refused",
        ),
        // `unknown` (the mod claimed the op but cannot prove the grant — its crash
        // window), `internal_error` (it died inside the handler), and every status
        // a future mod might invent. None of them establish that nothing was paid,
        // so none of them refund.
        _ => {
            tracing::error!(op_id, status, ack = %ack,
                "withdraw ack does not establish whether the payout landed; parking as 'stuck' for \
                 manual review (NOT refunding — the player may already hold the emeralds)");
            (
                WithdrawSettlement::Unknown,
                WithdrawFrom::NotParked,
                "unprovable",
            )
        }
    };
    apply_withdraw_settlement(conn, op_id, settlement, from, reason)
}

/// Apply `settlement` to a withdraw op and, if it owes one, refund — in ONE
/// transaction.
///
/// **The refund lives inside the state transition on purpose.** Guarding it with
/// a separate read (or with `set_state`) would leave a window in which
/// reconciliation and a late ack both see a refundable op and both refund it. So
/// the state filter is part of the UPDATE, and the eme moves only when this
/// statement is the one that changed the row.
///
/// `stuck` is not terminal (a late ack proving the payout may still settle it) but
/// it is never refundable — [`WithdrawFrom`] is what encodes that.
pub(super) fn apply_withdraw_settlement(
    conn: &mut Connection,
    op_id: &str,
    settlement: WithdrawSettlement,
    from: WithdrawFrom,
    reason: &str,
) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row = tx
        .query_row(
            "SELECT account_id, requested_amount, state, direction FROM emerald_ops WHERE op_id = ?1",
            [op_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((account_id, requested, state, direction)) = row else {
        tx.commit()?;
        tracing::warn!(op_id, reason, "withdraw settle: unknown op (dropping)");
        return Ok(());
    };
    // The mirror of the guard in `settle_ack`: refunding a CHARGE would credit an
    // account for emeralds it spent and never get them back.
    if direction != OpDirection::Withdraw.as_str() {
        tx.commit()?;
        tracing::error!(op_id, direction, reason,
            "withdraw settle: refusing to apply a WITHDRAW settlement to an op that is not a withdrawal");
        return Ok(());
    }
    if state == "settled" || state == "failed" {
        // Terminal — idempotent no-op. This is what makes a duplicated failure ack
        // refund exactly once: the first one moved the op to `failed` and paid the
        // eme back, and every repeat stops here.
        tx.commit()?;
        return Ok(());
    }

    let (new_state, settled_amount, refund) = match settlement {
        WithdrawSettlement::Granted(granted) if granted > 0 => {
            // More than was reserved means the mod paid out eme nobody debited.
            // We cannot take it back and must not debit again, so the ledger
            // records what this op was worth and the surplus is surfaced loudly.
            let granted = if granted > requested {
                tracing::error!(op_id, granted, requested, reason,
                    "withdraw ack reports MORE granted than was reserved — recording the reserved \
                     amount; the surplus was paid out against nothing (mod-side bug)");
                requested
            } else {
                granted
            };
            ("settled", Some(granted), requested - granted)
        }
        // A `Granted(0)` (or negative) is the same statement as `NotGranted`:
        // nothing was paid, so the whole reserve goes back.
        WithdrawSettlement::Granted(_) | WithdrawSettlement::NotGranted => {
            ("failed", Some(0), requested)
        }
        WithdrawSettlement::Unknown => ("stuck", None, 0),
    };

    // `from.sql()` is one of three compile-time constants; nothing external
    // reaches this string. COALESCE keeps `settled_amount` untouched when the
    // outcome does not know one (the `stuck` case).
    let changed = tx.execute(
        &format!(
            "UPDATE emerald_ops SET state = ?2, settled_amount = COALESCE(?3, settled_amount), \
               updated_unix_ms = ?4 \
             WHERE op_id = ?1 AND {}",
            from.sql()
        ),
        params![op_id, new_state, settled_amount, now_ms()],
    )?;
    if changed == 0 {
        // Something else already moved the op out of the states this settlement
        // may act on. That other transaction owns the outcome — including the
        // refund decision — so nothing is paid from here.
        tx.commit()?;
        if refund > 0 {
            tracing::error!(op_id, reason, state = %state, refund,
                "withdraw settle: NOT refunding — the op is no longer in a state this settlement may \
                 act on (a 'stuck' withdrawal may already have been granted and needs manual review, \
                 not an automatic refund)");
        } else {
            tracing::info!(op_id, reason, state = %state, new_state,
                "withdraw settle: op already moved on; leaving it as it is");
        }
        return Ok(());
    }
    if refund > 0 {
        // Reached only when the UPDATE above is what moved this op, so this runs
        // at most once per op no matter how many acks or reconcile passes arrive.
        wallet::refund_withdraw(&tx, &account_id, refund, now_ms())?;
    }
    tx.commit()?;
    match new_state {
        "settled" => tracing::info!(op_id, reason, granted = ?settled_amount, refunded = refund, "withdraw settled"),
        "failed" => tracing::warn!(op_id, reason, refunded = refund, "withdraw failed; reserve refunded"),
        _ => tracing::error!(op_id, reason,
            "withdraw parked as 'stuck' — the payout is unproven and the eme stays debited pending manual review"),
    }
    Ok(())
}

/// Dead-letter overdue withdrawals, one row at a time.
///
/// The charge pass can do this in two bulk UPDATEs because neither of its
/// outcomes owes anybody money. A withdrawal's `failed` does: the eme was debited
/// up front, so failing the op and refunding it are the same act and have to
/// happen in one transaction. Hence a row at a time, through
/// [`apply_withdraw_settlement`].
pub(super) fn dead_letter_withdrawals(conn: &mut Connection, cutoff: i64) {
    let overdue = {
        let mut stmt = match conn.prepare(
            "SELECT op_id, state FROM emerald_ops \
             WHERE direction = 'withdraw' AND state IN ('pending','sent') AND created_unix_ms < ?1 \
             ORDER BY created_unix_ms ASC LIMIT 50",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "reconcile: dead-letter withdraw prepare failed");
                return;
            }
        };
        match stmt
            .query_map([cutoff], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .and_then(|m| m.collect::<rusqlite::Result<Vec<_>>>())
        {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "reconcile: dead-letter withdraw query failed");
                return;
            }
        }
    };

    for (op_id, state) in overdue {
        let (settlement, from, reason) = match state.as_str() {
            // Never delivered ⇒ no emeralds were granted ⇒ the reserve is the
            // player's and goes back.
            "pending" => {
                tracing::warn!(op_id, "reconcile: dead-lettering an undelivered withdrawal -> failed + refund");
                (
                    WithdrawSettlement::NotGranted,
                    WithdrawFrom::NeverSent,
                    "dead_letter_pending",
                )
            }
            // It reached the mod and we never learned the outcome. Refunding here
            // would risk paying twice, so it goes to a human instead.
            "sent" => {
                tracing::error!(op_id, "reconcile: dead-lettering a delivered withdrawal -> stuck \
                    (payout unproven after 24h; needs manual review, NOT an automatic refund)");
                (
                    WithdrawSettlement::Unknown,
                    WithdrawFrom::NotParked,
                    "dead_letter_sent",
                )
            }
            // Unreachable while the query above selects only those two, and left
            // in place rather than guessed at if that ever changes.
            other => {
                tracing::error!(op_id, state = other,
                    "reconcile: dead-letter withdraw selected an unexpected state; leaving it alone");
                continue;
            }
        };
        if let Err(e) = apply_withdraw_settlement(conn, &op_id, settlement, from, reason) {
            tracing::error!(error = %e, op_id, reason,
                "reconcile: dead-letter withdraw failed to apply; the op is unchanged and stays open");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charge::test_support::*;
    use crate::charge::{charge_scope, DEAD_LETTER_MS};

    #[test]
    fn withdraw_scope_isolates_accounts_and_directions() {
        assert_ne!(withdraw_scope("acct-a"), withdraw_scope("acct-b"));
        assert_eq!(withdraw_scope("acct-a"), withdraw_scope("acct-a"));
        // The consequential one: the same key on the same account must not reach
        // across the two directions.
        assert_ne!(withdraw_scope("acct-a"), charge_scope("acct-a"));
        // …nor collide with the transfer scopes sharing the (idem_key, scope) PK.
        assert_ne!(withdraw_scope("acct-a"), "send");
        assert_ne!(withdraw_scope("acct-a"), "pay");
        assert_ne!(withdraw_scope("acct-a"), "charge");
        assert_ne!(withdraw_scope("acct-a"), "withdraw");
    }

    const UUID: &str = "11111111-2222-4333-8444-555555555555";

    #[tokio::test]
    async fn begin_withdraw_debits_first_and_leaves_a_pollable_op() {
        let (pool, coord) = coordinator();
        fund(&pool, 10_000);

        let v = coord
            .begin_withdraw("k1", "acct-a", UUID, "mc1", 4_000)
            .await
            .unwrap();
        assert_eq!(v["ok"], json!(true));
        // The tunnel is down, so nothing was sent and the op waits for
        // reconciliation — with the eme already debited, which is the point.
        assert_eq!(v["state"], "pending");
        assert_eq!(balance_of(&pool), 6_000);
        assert_eq!(txns(&pool), vec![("withdraw".to_string(), -4_000)]);
        let op_id = v["op_id"].as_str().unwrap().to_string();
        assert_eq!(state_of(&pool, &op_id), "pending");

        // A retry of the same key replays that op instead of debiting again.
        let again = coord
            .begin_withdraw("k1", "acct-a", UUID, "mc1", 4_000)
            .await
            .unwrap();
        assert_eq!(again["op_id"], json!(op_id));
        assert_eq!(again["duplicate"], json!(true));
        assert_eq!(balance_of(&pool), 6_000);
        assert_eq!(txns(&pool).len(), 1);
    }

    #[tokio::test]
    async fn a_withdrawal_that_cannot_proceed_writes_nothing_at_all() {
        let (pool, coord) = coordinator();
        fund(&pool, 10_000);

        let short = coord
            .begin_withdraw("k-short", "acct-a", UUID, "mc1", 10_100)
            .await
            .unwrap();
        assert_eq!(short["ok"], json!(false));
        assert_eq!(short["error"], "insufficient");
        assert_eq!(short["balance"], json!(10_000));

        // A bad destination is refused BEFORE the debit, unlike a charge (which
        // creates the op first) — nothing is owed back because nothing was taken.
        let bad_uuid = coord
            .begin_withdraw("k-uuid", "acct-a", "not-a-uuid", "mc1", 1_000)
            .await
            .unwrap();
        assert_eq!(bad_uuid["error"], "bad_uuid");

        // One minor unit over the bound is still over it: the ceiling is on the
        // ledger amount, and a withdrawal is refused before anything asks whether
        // that amount is a whole number of emeralds.
        let over = coord
            .begin_withdraw("k-max", "acct-a", UUID, "mc1", wallet::MAX_WITHDRAW_PER_OP + 1)
            .await
            .unwrap();
        assert_eq!(over["error"], "bad_amount");

        assert_eq!(balance_of(&pool), 10_000);
        assert!(txns(&pool).is_empty());
        // No idempotency record either: the user must be able to top up and reuse
        // the very same key.
        for key in ["k-short", "k-uuid", "k-max"] {
            assert!(
                coord.replay_withdraw("acct-a", key).await.unwrap().is_none(),
                "{key} left a replay record behind"
            );
        }
    }

    #[test]
    fn a_granted_ack_settles_without_moving_the_balance_again() {
        let (pool, _coord) = coordinator();
        fund(&pool, 0); // the 10 エメ are already reserved by the op below
        insert_withdraw(&pool, "op-w", "sent", 1_000, false);

        // 10 emeralds, which is the 1,000 minor units the op reserved: the ack
        // speaks the mod's unit and the ledger records its own.
        withdraw_ack(
            &pool,
            json!({ "op_id": "op-w", "status": "ok", "granted": 10 }),
        );

        assert_eq!(state_of(&pool, "op-w"), "settled");
        assert_eq!(settled_of(&pool, "op-w"), Some(1_000));
        // The debit stands: the player has the emeralds.
        assert_eq!(balance_of(&pool), 0);
        assert!(txns(&pool).is_empty());
    }

    #[test]
    fn a_partial_grant_refunds_only_the_shortfall() {
        let (pool, _coord) = coordinator();
        fund(&pool, 0);
        insert_withdraw(&pool, "op-w", "sent", 1_000, false);

        // Shouldn't happen, but the ack states exactly what was paid, so the rest
        // is provably unpaid and goes back. 4 emeralds of the 10 reserved ⇒ 400
        // minor settled, 600 refunded — the shortfall is computed AFTER the two
        // sides are in the same unit, which is the arithmetic this pins.
        withdraw_ack(
            &pool,
            json!({ "op_id": "op-w", "status": "duplicate", "granted": 4 }),
        );

        assert_eq!(state_of(&pool, "op-w"), "settled");
        assert_eq!(settled_of(&pool, "op-w"), Some(400));
        assert_eq!(balance_of(&pool), 600);
        assert_eq!(txns(&pool), vec![("withdraw".to_string(), 600)]);
    }

    #[test]
    fn the_same_failure_ack_refunds_exactly_once() {
        // THE test. At-least-once delivery means the mod's answer can arrive more
        // than once, and reconciliation re-drives ops on a timer; a refund that is
        // not idempotent mints eme on every repeat.
        let (pool, _coord) = coordinator();
        fund(&pool, 0);
        insert_withdraw(&pool, "op-w", "sent", 1_000, false);

        let ack = json!({ "op_id": "op-w", "status": "player_offline", "granted": 0 });
        for _ in 0..5 {
            withdraw_ack(&pool, ack.clone());
        }

        assert_eq!(state_of(&pool, "op-w"), "failed");
        assert_eq!(settled_of(&pool, "op-w"), Some(0));
        assert_eq!(balance_of(&pool), 1_000);
        assert_eq!(txns(&pool), vec![("withdraw".to_string(), 1_000)]);
    }

    #[test]
    fn an_unprovable_ack_parks_the_op_and_never_refunds() {
        let (pool, _coord) = coordinator();
        fund(&pool, 0);
        insert_withdraw(&pool, "op-unknown", "sent", 1_000, false);
        insert_withdraw(&pool, "op-internal", "sent", 1_000, false);
        insert_withdraw(&pool, "op-novel", "sent", 1_000, false);

        // `unknown` = the mod claimed the op but cannot prove it paid (its crash
        // window). `internal_error` = it died inside the handler. A status this
        // build has never heard of is the same situation.
        withdraw_ack(
            &pool,
            json!({ "op_id": "op-unknown", "status": "unknown", "granted": 0 }),
        );
        withdraw_ack(
            &pool,
            json!({ "op_id": "op-internal", "status": "internal_error" }),
        );
        withdraw_ack(
            &pool,
            json!({ "op_id": "op-novel", "status": "grant_deferred" }),
        );

        for op in ["op-unknown", "op-internal", "op-novel"] {
            assert_eq!(state_of(&pool, op), "stuck", "{op}");
            assert_eq!(settled_of(&pool, op), None, "{op}");
        }
        assert_eq!(balance_of(&pool), 0);
        assert!(txns(&pool).is_empty());
    }

    #[test]
    fn a_success_without_a_readable_amount_parks_instead_of_refunding() {
        // The trap the charge settler's `unwrap_or(0)` would spring here: the mod
        // says it PAID, so reading a missing/garbled amount as zero would refund a
        // payout the player is holding — emeralds and eme both.
        let (pool, _coord) = coordinator();
        fund(&pool, 0);
        insert_withdraw(&pool, "op-missing", "sent", 1_000, false);
        insert_withdraw(&pool, "op-fractional", "sent", 1_000, false);
        insert_withdraw(&pool, "op-float", "sent", 1_000, false);

        withdraw_ack(&pool, json!({ "op_id": "op-missing", "status": "ok" }));
        withdraw_ack(
            &pool,
            json!({ "op_id": "op-fractional", "status": "ok", "granted": 9.5 }),
        );
        // …while a fractionless float IS an emerald count (Gson writes 10.0).
        withdraw_ack(
            &pool,
            json!({ "op_id": "op-float", "status": "ok", "granted": 10.0 }),
        );

        assert_eq!(state_of(&pool, "op-missing"), "stuck");
        assert_eq!(state_of(&pool, "op-fractional"), "stuck");
        assert_eq!(state_of(&pool, "op-float"), "settled");
        assert_eq!(settled_of(&pool, "op-float"), Some(1_000));
        assert_eq!(balance_of(&pool), 0);
        assert!(txns(&pool).is_empty());
    }

    #[test]
    fn a_parked_withdrawal_can_still_settle_but_can_never_auto_refund() {
        let (pool, _coord) = coordinator();
        fund(&pool, 0);
        insert_withdraw(&pool, "op-w", "stuck", 1_000, false);

        // A failure ack arriving late must NOT refund: `stuck` means the payout may
        // already be in the player's inventory, and paying the eme back too would
        // hand them both.
        withdraw_ack(
            &pool,
            json!({ "op_id": "op-w", "status": "player_offline", "granted": 0 }),
        );
        assert_eq!(state_of(&pool, "op-w"), "stuck");
        assert_eq!(balance_of(&pool), 0);
        assert!(txns(&pool).is_empty());

        // A late ack that PROVES the payout is the correct end for it.
        withdraw_ack(
            &pool,
            json!({ "op_id": "op-w", "status": "duplicate", "granted": 10 }),
        );
        assert_eq!(state_of(&pool, "op-w"), "settled");
        assert_eq!(balance_of(&pool), 0);
        assert!(txns(&pool).is_empty());
    }

    #[tokio::test]
    async fn the_dead_letter_pass_refunds_undelivered_withdrawals_once_and_parks_delivered_ones() {
        let (pool, coord) = coordinator();
        fund(&pool, 0); // 10 + 7 エメ are reserved by the two withdrawals below
        insert_withdraw(&pool, "w-pending", "pending", 1_000, true);
        insert_withdraw(&pool, "w-sent", "sent", 700, true);
        insert_full_op(
            &pool,
            "c-pending",
            Some("mc1"),
            "charge",
            "pending",
            1_000,
            now_ms() - DEAD_LETTER_MS - 1_000,
        );
        insert_full_op(
            &pool,
            "c-sent",
            Some("mc1"),
            "charge",
            "sent",
            1_000,
            now_ms() - DEAD_LETTER_MS - 1_000,
        );
        // A fresh withdrawal is not overdue and must be left alone.
        insert_withdraw(&pool, "w-fresh", "pending", 500, false);

        // Twice: an ageing op is revisited on every cycle, so the refund has to be
        // a one-off, not a per-pass event.
        coord.reconcile().await;
        coord.reconcile().await;

        // Never delivered ⇒ nothing granted ⇒ failed, and the reserve goes back.
        assert_eq!(state_of(&pool, "w-pending"), "failed");
        // Delivered, outcome unproven ⇒ parked for a human, NOT refunded.
        assert_eq!(state_of(&pool, "w-sent"), "stuck");
        assert_eq!(settled_of(&pool, "w-sent"), None);
        // The charge rows keep the behaviour they always had.
        assert_eq!(state_of(&pool, "c-pending"), "failed");
        assert_eq!(state_of(&pool, "c-sent"), "stuck");
        // The fresh one is still open (the tunnel is down, so its re-drive is a
        // no-op that leaves it where it was).
        assert_eq!(state_of(&pool, "w-fresh"), "pending");

        assert_eq!(balance_of(&pool), 1_000);
        assert_eq!(txns(&pool), vec![("withdraw".to_string(), 1_000)]);
    }

    #[tokio::test]
    async fn an_undeliverable_withdrawal_is_refunded_rather_than_stranded() {
        // `set_state` cannot be used to fail a withdrawal — it moves no money, so
        // the eme would stay debited with nothing left to refund it. This is the
        // path where that would be easiest to get wrong.
        let (pool, coord) = coordinator();
        fund(&pool, 0);
        insert_full_op(&pool, "w-null-pending", None, "withdraw", "pending", 1_000, now_ms());
        insert_full_op(&pool, "w-null-sent", None, "withdraw", "sent", 700, now_ms());
        // A direction nothing knows how to drive: escalated, never guessed at, and
        // never refunded (an unknown direction says nothing about what was
        // reserved).
        insert_full_op(&pool, "w-alien", Some("mc1"), "transmute", "pending", 300, now_ms());

        coord.reconcile().await;

        assert_eq!(state_of(&pool, "w-null-pending"), "failed");
        assert_eq!(state_of(&pool, "w-null-sent"), "stuck");
        assert_eq!(state_of(&pool, "w-alien"), "stuck");
        assert_eq!(balance_of(&pool), 1_000);
        assert_eq!(txns(&pool), vec![("withdraw".to_string(), 1_000)]);
    }
}
