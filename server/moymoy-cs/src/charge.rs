//! Emerald ledger coordinator: the bridge between the in-world mod (truth of what
//! happens to emeralds) and the wallet (truth of balance), reconciled through the
//! `emerald_ops` ledger. It runs BOTH directions — `charge` (emeralds → eme) and
//! `withdraw` (eme → emeralds).
//!
//! Consistency model (DEV.md): at-least-once delivery + an op-keyed idempotent
//! settlement, with the irreversible half done FIRST. A reconciliation pass
//! re-sends non-terminal ops so a dropped request/ack still eventually settles
//! (the mod is op-idempotent and re-acks).
//!
//! ## The safe direction of failure is different for each direction
//!
//! This is the thing to hold on to when changing anything here. Both flows have a
//! step that mints value if it happens without its counterpart, and each one does
//! that step first so the failure mode is a debt to reconcile rather than value
//! from nowhere:
//!
//! | | first | then | if the second never happens |
//! |---|---|---|---|
//! | charge | mod consumes | wallet credits | the player is owed eme (settle late) |
//! | withdraw | wallet debits | mod grants | the player is owed eme (refund) |
//!
//! So the balance is credited on a charge ONLY when the mod's ack says emeralds
//! were consumed (a lost ack never mints eme that no emerald paid for, a
//! duplicate ack never double-credits), and a withdrawal debits BEFORE asking for
//! a payout (granting first would mean a failed debit leaves emeralds nothing
//! paid for). And the dangerous unknown flips with it: for a charge, "did it
//! consume?" must not be answered "no" and written off; for a withdrawal, "did it
//! grant?" must not be answered "no" and refunded — that would hand the player
//! the emeralds AND the eme. Unknown is its own state (`stuck`), never rounded to
//! either side.
//!
//! Since the move to HTTP in MNN (MochiOS DEV.md §7.3.10) the ack arrives as the
//! **response to the request**, so the common case settles inside
//! [`ChargeCoordinator::begin_charge`] / [`ChargeCoordinator::begin_withdraw`]
//! instead of minutes later on an inbound frame. The ledger is unchanged and
//! still load-bearing: an exchange that fails after the mod acted is
//! [`ChargeOutcome::Ambiguous`], and only reconciliation — driving the same
//! `op_id` until the mod re-acks — can close it.
//!
//! What is shared by both directions lives here: the coordinator, the op
//! direction, `drive`, reconciliation and the charge settlement. The withdraw
//! half — its reserve, its `granted` ack and its refund rules — is in the
//! [`withdraw`] child module.

mod withdraw;

/// Re-exported so the settler keeps its `charge::settle_withdraw_ack` path — the
/// name `mc.rs` refers to, and the mirror of [`settle_ack`] next to it.
pub use withdraw::settle_withdraw_ack;

#[cfg(test)]
mod test_support;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::db::{self, now_ms, Pool};
use crate::error::ApiError;
use crate::mc::{ChargeOutcome, McLink};
use crate::wallet;

use withdraw::{dead_letter_withdrawals, WithdrawFrom, WithdrawSettlement};

/// Charge-txn label so a real emerald charge is distinguishable in the ledger.
const CHARGE_LABEL: &str = "インベントリのエメラルド";

/// A non-terminal op older than this is dead-lettered by reconciliation (R008).
/// The asymmetry is the same in both directions — a never-delivered `pending` op
/// is safe to terminate (a charge consumed nothing; a withdrawal granted nothing,
/// so its reserve is refunded), while a `sent` op did reach the mod and becomes
/// `stuck` for manual review. Never auto-resolved, so neither consumed emeralds
/// nor an already-granted payout is written off.
const DEAD_LETTER_MS: i64 = 24 * 60 * 60 * 1000;

/// Which way an `emerald_ops` row runs.
///
/// A type rather than a string wherever an op is driven, because the two verbs
/// are opposites: re-sending a withdrawal as `emerald.charge` would CONFISCATE
/// the emeralds it was supposed to pay out. A caller cannot express that mistake
/// by passing the wrong literal, and a row whose stored direction is neither of
/// these is escalated rather than guessed at (see [`ChargeCoordinator::reconcile`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OpDirection {
    Charge,
    Withdraw,
}

impl OpDirection {
    /// The value stored in `emerald_ops.direction`.
    fn as_str(self) -> &'static str {
        match self {
            OpDirection::Charge => "charge",
            OpDirection::Withdraw => "withdraw",
        }
    }

    /// Read the ledger's column. Unknown ⇒ `None` (fail closed — never a default,
    /// because the default would be a verb with an in-world effect).
    fn parse(s: &str) -> Option<Self> {
        match s {
            "charge" => Some(OpDirection::Charge),
            "withdraw" => Some(OpDirection::Withdraw),
            _ => None,
        }
    }
}

/// Internal outcome of the begin-charge transaction.
enum BeginCharge {
    /// A prior op exists for this idem_key — replay it.
    Existing(String),
    /// A fresh op was created.
    Fresh(String),
}

/// The idempotency scope for a charge: `charge:<account_id>`.
///
/// Including the account is what stops one account from reaching another's op.
/// With a bare `"charge"` scope, an attacker who guessed (or was told) a victim's
/// `idem_key` could POST it and receive the victim's `op_id` — and the victim's
/// next legitimate retry, sharing the key, would then replay against an op the
/// attacker had already driven. The scope is a string in a `(idem_key, scope)`
/// primary key, so widening it costs no schema surgery.
pub fn charge_scope(account_id: &str) -> String {
    format!("charge:{account_id}")
}

/// One non-terminal op a reconciliation pass picked up. `attester_id` is
/// `Option` because the column is nullable for rows written before schema v5 —
/// see [`ChargeCoordinator::reconcile`] for why such a row is escalated rather
/// than re-driven. `direction` decides which verb re-drives it and is therefore
/// read from the row, never assumed.
struct PendingOp {
    op_id: String,
    idem_key: String,
    mc_uuid: String,
    attester_id: Option<String>,
    direction: String,
    amount: i64,
    state: String,
}

/// Player inventory snapshot for the charge screen (9 emeralds = 1 block).
///
/// `reachable`/`online` keep the three real outcomes distinct instead of
/// collapsing them to "0 emeralds": `reachable=false` ⇒ the mod never answered
/// (offline / server doesn't host moymoy / MC connector down); `online=false` ⇒
/// the mod answered but the UUID isn't a live player there (a UUID mismatch shows
/// up here, NOT as a genuine zero balance).
///
/// **`emeralds` and `blocks` are item counts; `chargeable` is money.** They are
/// the same numbers in different units and are served side by side, so the field
/// that is an amount is in minor units like every other amount and the two that
/// are stacks of items are not.
#[derive(Debug)]
pub struct Inventory {
    pub reachable: bool,
    pub online: bool,
    pub emeralds: i64,
    pub blocks: i64,
    pub chargeable: i64,
}

/// What an inventory of `emeralds` loose emeralds and `blocks` emerald blocks is
/// worth, in minor units.
///
/// Saturating rather than checked: this is the number on a charge screen, and a
/// mod reporting an impossible inventory is a display problem, not a settlement
/// one — nothing is credited from here, and what a charge actually moves is
/// bounded by the request and by the mod's own ack. Wrapping would show a
/// negative balance instead.
fn chargeable_minor(emeralds: i64, blocks: i64) -> i64 {
    blocks
        .saturating_mul(9)
        .saturating_add(emeralds)
        .saturating_mul(crate::mc::MINOR_PER_EMERALD)
}

/// Drives emerald charges over the backend's cs tunnel. Holds the SQLite pool
/// (for the `emerald_ops` ledger) and the MC link.
pub struct ChargeCoordinator {
    pool: Pool,
    mc: McLink,
}

impl ChargeCoordinator {
    pub fn new(pool: Pool, mc: McLink) -> Self {
        ChargeCoordinator { pool, mc }
    }

    /// Whether emerald charging is available right now — i.e. whether the cs
    /// tunnel is live. Charging rides the SAME tunnel as the wallet's own inbound
    /// HTTP, so this is pure liveness; there is no separate credential (and no
    /// "configured / not configured" axis) left to report.
    pub fn can_charge(&self) -> bool {
        self.mc.is_connected()
    }

    /// Query a Minecraft character's chargeable inventory via the mod on
    /// `attester_id`. Both come from a Hub-signed assertion the account confirmed
    /// (`crate::attest`), never from the request. Only reached when
    /// `can_charge()` is true.
    pub async fn query_inventory(
        &self,
        attester_id: &str,
        mc_uuid: &str,
    ) -> Result<Inventory, ApiError> {
        let uuid =
            Uuid::parse_str(mc_uuid).map_err(|_| ApiError::bad_request("mc_uuid is not a UUID"))?;
        match self.mc.query_inventory(attester_id, &uuid).await {
            Some((online, emeralds, blocks)) => Ok(Inventory {
                reachable: true,
                online,
                emeralds,
                blocks,
                chargeable: chargeable_minor(emeralds, blocks),
            }),
            // No round-trip: keep it distinct from a real zero so the UI can say
            // WHY (character offline / not in-game / MC-side not set up).
            None => Ok(Inventory {
                reachable: false,
                online: false,
                emeralds: 0,
                blocks: 0,
                chargeable: 0,
            }),
        }
    }

    /// The frozen response for an existing `(account_id, idem_key)` charge, if
    /// there is one.
    ///
    /// **The one charge path that needs no assertion.** A replay cannot produce a
    /// new consumption — it hands back the op the first call already created — so
    /// asking the user to approve it again would be a consent prompt for
    /// something that already happened. That matters in practice: without this,
    /// every retry after a lost response or a poll timeout would raise a second
    /// modal for the same charge.
    ///
    /// `Ok(None)` means no prior op, and the caller must go through the full
    /// attestation path.
    pub async fn replay_charge(
        &self,
        account_id: &str,
        idem_key: &str,
    ) -> Result<Option<Value>, ApiError> {
        self.replay_op(charge_scope(account_id), "charge_failed", idem_key)
            .await
    }

    /// Shared body of the two replays: look the frozen response up in `scope` and
    /// hand back the op it names. A record that cannot yield an `op_id` is
    /// reported as `error` — never as a success, which would tell the app an op
    /// exists that it can then never poll.
    async fn replay_op(
        &self,
        scope: String,
        error: &'static str,
        idem_key: &str,
    ) -> Result<Option<Value>, ApiError> {
        let pool = self.pool.clone();
        let ik = idem_key.to_string();
        tokio::task::spawn_blocking(move || -> Result<Option<Value>, ApiError> {
            let conn = pool.get()?;
            let Some(prev) = db::idem_get(&conn, &ik, &scope)? else {
                return Ok(None);
            };
            match serde_json::from_str::<Value>(&prev) {
                Ok(v) => {
                    let op_id = v.get("op_id").and_then(Value::as_str).unwrap_or("");
                    if op_id.is_empty() {
                        tracing::error!(idem_key = %ik, %scope, error,
                            "replay: idempotency record has no op_id; reporting a failure rather than fabricating success");
                        return Ok(Some(json!({ "ok": false, "error": error })));
                    }
                    Ok(Some(json!({ "ok": true, "op_id": op_id, "state": "pending", "duplicate": true })))
                }
                Err(e) => {
                    tracing::error!(error = %e, idem_key = %ik, %scope, code = error,
                        "replay: corrupt idempotency record; reporting a failure rather than fabricating success");
                    Ok(Some(json!({ "ok": false, "error": error })))
                }
            }
        })
        .await?
    }

    /// Begin an emerald charge: record a pending `emerald_ops` row (idempotent on
    /// `(account_id, idem_key)`), ask the mod on `attester_id` to consume, and
    /// return a pollable op (`GET /wallet/op`). The balance is credited from the
    /// mod's ack — to `account_id` (the MoyMoy account), while the consumption is
    /// addressed to `attester_id` and `mc_uuid` (the server and character the
    /// caller's assertion named).
    ///
    /// The ack is normally the charge request's own HTTP response, so the op this
    /// returns is usually already `settled`. It stays pollable because the other
    /// outcomes (server unreachable, tunnel down, ambiguous) settle later, on a
    /// reconciliation pass.
    pub async fn begin_charge(
        &self,
        idem_key: &str,
        account_id: &str,
        mc_uuid: &str,
        attester_id: &str,
        amount: i64,
    ) -> Result<Value, ApiError> {
        if amount <= 0 || amount > wallet::MAX_AMOUNT {
            return Ok(json!({ "ok": false, "error": "bad_amount" }));
        }

        // 1. Create (or replay) the op in one transaction.
        let pool = self.pool.clone();
        let ik = idem_key.to_string();
        let aid = account_id.to_string();
        let scope = charge_scope(account_id);
        let muuid = mc_uuid.to_string();
        let att = attester_id.to_string();
        let outcome = tokio::task::spawn_blocking(move || -> Result<BeginCharge, ApiError> {
            let mut conn = pool.get()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(prev) = db::idem_get(&tx, &ik, &scope)? {
                let op = match serde_json::from_str::<Value>(&prev) {
                    Ok(v) => v.get("op_id").and_then(Value::as_str).unwrap_or("").to_string(),
                    Err(e) => {
                        tracing::error!(error = %e, idem_key = %ik,
                            "begin_charge: corrupt idempotency record; treating as charge_failed (not fabricating success)");
                        String::new()
                    }
                };
                return Ok(BeginCharge::Existing(op));
            }
            let op_id = Uuid::new_v4().to_string();
            let now = now_ms();
            // attester_id is persisted with the op, not re-derived later: a retry
            // hours from now must reach the server the user consented to, not
            // whichever one they happen to be on then.
            tx.execute(
                "INSERT INTO emerald_ops \
                   (op_id, idem_key, account_id, mc_uuid, attester_id, direction, requested_amount, settled_amount, state, created_unix_ms, updated_unix_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 'pending', ?8, ?8)",
                params![op_id, ik, aid, muuid, att, OpDirection::Charge.as_str(), amount, now],
            )?;
            let resp = json!({ "ok": true, "op_id": op_id, "state": "pending" });
            db::idem_put(&tx, &ik, &scope, &resp.to_string())?;
            tx.commit()?;
            Ok(BeginCharge::Fresh(op_id))
        })
        .await??;

        let (op_id, fresh) = match outcome {
            BeginCharge::Existing(op) => (op, false),
            BeginCharge::Fresh(op) => (op, true),
        };

        if op_id.is_empty() {
            return Ok(json!({ "ok": false, "error": "charge_failed" }));
        }
        if !fresh {
            return Ok(
                json!({ "ok": true, "op_id": op_id, "state": "pending", "duplicate": true }),
            );
        }

        // 2. Ask the mod on the consented server to consume, and settle its ack.
        let uuid = match Uuid::parse_str(mc_uuid) {
            Ok(u) => u,
            Err(_) => {
                self.set_state(&op_id, "failed").await;
                return Ok(json!({ "ok": false, "error": "bad_uuid", "op_id": op_id }));
            }
        };
        // The op was just inserted as 'pending', and the outcomes that report no
        // transition leave it exactly there: nothing was consumed, and the
        // reconciliation pass retries it.
        let state = self
            .drive(
                OpDirection::Charge,
                attester_id,
                &uuid,
                &op_id,
                idem_key,
                amount,
                "begin_charge",
            )
            .await?
            .unwrap_or_else(|| "pending".to_string());
        Ok(json!({ "ok": true, "op_id": op_id, "state": state }))
    }

    /// Send one attempt for `op_id` and fold its outcome into the ledger. Shared
    /// by the primary path and reconciliation so a retry can never take a
    /// different route than the original — the mod is `op_id`-idempotent, so both
    /// are the same operation.
    ///
    /// `direction` picks BOTH the verb and the settler, so the two can never be
    /// mismatched; it comes from the caller's own INSERT on the primary path and
    /// from the ledger row on a retry, never from a guess.
    ///
    /// Returns the op's new state when this attempt moved it, `None` when it left
    /// the op untouched. The transitions encode **whether the mod may have acted
    /// in-world**:
    ///
    /// * acked → settled / failed by the direction's settler (the only path that
    ///   moves a balance)
    /// * server unreachable / nothing sent → untouched (nothing happened, so a
    ///   stale `pending` is safe for the dead-letter pass to terminate — failing a
    ///   charge, refunding a withdrawal)
    /// * ambiguous → `sent`, which the dead-letter pass escalates to `stuck` for
    ///   manual review rather than writing consumed emeralds off or refunding a
    ///   payout that may have landed
    ///
    /// `Err` is the fourth, and it is NOT one of those transitions: `amount` could
    /// not be expressed as a number of emeralds, so nothing was built and nothing
    /// was sent (`crate::mc::to_physical`). It surfaces rather than being folded
    /// into `None`, because "nothing happened this time" and "nothing can ever
    /// happen for this amount" want different answers — the first is a retry, the
    /// second is a `500` the caller sees and an op the dead-letter pass terminates
    /// on the safe side, exactly as it terminates one that never left.
    #[allow(clippy::too_many_arguments)]
    async fn drive(
        &self,
        direction: OpDirection,
        attester_id: &str,
        uuid: &Uuid,
        op_id: &str,
        idem_key: &str,
        amount: i64,
        origin: &str,
    ) -> Result<Option<String>, ApiError> {
        let outcome = match direction {
            OpDirection::Charge => {
                self.mc
                    .send_charge(attester_id, uuid, op_id, idem_key, amount)
                    .await?
            }
            OpDirection::Withdraw => {
                self.mc
                    .send_withdraw(attester_id, uuid, op_id, idem_key, amount)
                    .await?
            }
        };
        Ok(match outcome {
            ChargeOutcome::Acked(ack) => Some(self.settle_now(direction, op_id, ack).await),
            ChargeOutcome::ServerUnreachable | ChargeOutcome::NotSent => None,
            ChargeOutcome::Ambiguous(msg) => {
                tracing::warn!(op_id, origin, direction = direction.as_str(), error = %msg,
                    "emerald op exchange failed with its in-world effect UNKNOWN; marking 'sent' so reconciliation re-drives it");
                self.set_state(op_id, "sent").await;
                Some("sent".to_string())
            }
        })
    }

    /// Apply the mod's ack to the ledger and read back the op's resulting state,
    /// in ONE blocking hop so the state reported is the one this ack produced.
    async fn settle_now(&self, direction: OpDirection, op_id: &str, ack: Value) -> String {
        let pool = self.pool.clone();
        let op = op_id.to_string();
        let joined = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get()?;
            // Each settler also refuses acks aimed at the other direction's rows,
            // so a mixed-up ack cannot settle the wrong op even from here.
            match direction {
                OpDirection::Charge => settle_ack(&mut conn, &ack)?,
                OpDirection::Withdraw => settle_withdraw_ack(&mut conn, &ack)?,
            }
            Ok::<Option<String>, ApiError>(
                op_view(&conn, &op)?
                    .and_then(|(_, v)| v.get("state").and_then(Value::as_str).map(str::to_string)),
            )
        })
        .await;

        let failure = match joined {
            Ok(Ok(Some(state))) => return state,
            Ok(Ok(None)) => "the op is not in the ledger".to_string(),
            Ok(Err(e)) => e.to_string(),
            Err(e) => format!("spawn_blocking join failed (panic or shutdown): {e}"),
        };
        // The mod ANSWERED, so it may already have acted and we merely failed to
        // record it. Park the op in 'sent' — the state that means "in-world effect
        // unknown" — so reconciliation re-drives it (the mod re-acks `duplicate`
        // with the same amount) and, if it never closes, the dead-letter pass
        // escalates it to 'stuck' for review. Right in both directions: it neither
        // writes consumed emeralds off as never-delivered, nor refunds a payout
        // that may be sitting in the player's inventory.
        tracing::error!(op_id, direction = direction.as_str(), error = %failure,
            "settle: could not record the mod's ack; parking the op as 'sent' for reconciliation");
        self.set_state(op_id, "sent").await;
        "sent".to_string()
    }

    /// Re-send non-terminal ops so a dropped request/ack eventually settles
    /// (at-least-once; the mod is op-idempotent and re-acks with the same
    /// consumed amount). Called on a timer + at startup from `main`.
    ///
    /// Runs regardless of tunnel liveness: the dead-letter pass below must age
    /// ops out even while the tunnel is down, and a re-send that finds it down
    /// simply reports `NotSent` and leaves the op for the next cycle.
    pub async fn reconcile(&self) {
        let pool = self.pool.clone();
        let ops: Vec<PendingOp> = match tokio::task::spawn_blocking(move || {
            let mut conn = match pool.get() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "reconcile: pool.get failed");
                    return Vec::new();
                }
            };
            // R008: dead-letter ops too old to keep retrying. A never-delivered
            // `pending` op (nothing happened in-world) is safe to terminate; a
            // `sent` op (in-world effect ambiguous) goes to `stuck` for manual
            // review — never auto-resolved, so consumed emeralds aren't written
            // off and a landed payout isn't refunded. A late ack can still settle
            // a `stuck` op (neither settler skips it).
            let cutoff = now_ms() - DEAD_LETTER_MS;
            // `AND direction = 'charge'`: a charge terminates with nothing owed to
            // anyone, so both cases are one UPDATE each. A withdrawal's `failed`
            // carries a refund obligation that a bulk UPDATE cannot discharge —
            // those rows are handled one at a time, below.
            match conn.execute(
                "UPDATE emerald_ops SET state = 'failed', updated_unix_ms = ?2 \
                 WHERE state = 'pending' AND direction = 'charge' AND created_unix_ms < ?1",
                params![cutoff, now_ms()],
            ) {
                Ok(n) if n > 0 => {
                    tracing::warn!(count = n, "reconcile: dead-lettered stale pending charges -> failed (never delivered)")
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "reconcile: dead-letter pending failed"),
            }
            match conn.execute(
                "UPDATE emerald_ops SET state = 'stuck', updated_unix_ms = ?2 \
                 WHERE state = 'sent' AND direction = 'charge' AND created_unix_ms < ?1",
                params![cutoff, now_ms()],
            ) {
                Ok(n) if n > 0 => {
                    tracing::error!(count = n, "reconcile: dead-lettered stale sent charges -> stuck (consumption ambiguous; needs manual review)")
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "reconcile: dead-letter sent failed"),
            }
            dead_letter_withdrawals(&mut conn, cutoff);
            let mut stmt = match conn.prepare(
                "SELECT op_id, idem_key, mc_uuid, attester_id, direction, requested_amount, state \
                 FROM emerald_ops \
                 WHERE state IN ('pending','sent') ORDER BY created_unix_ms ASC LIMIT 50",
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "reconcile: prepare failed");
                    return Vec::new();
                }
            };
            match stmt
                .query_map([], |r| {
                    Ok(PendingOp {
                        op_id: r.get(0)?,
                        idem_key: r.get(1)?,
                        mc_uuid: r.get(2)?,
                        attester_id: r.get(3)?,
                        direction: r.get(4)?,
                        amount: r.get(5)?,
                        state: r.get(6)?,
                    })
                })
                .and_then(|m| m.collect::<rusqlite::Result<Vec<_>>>())
            {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = %e, "reconcile: query failed");
                    Vec::new()
                }
            }
        })
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "reconcile: spawn_blocking join failed (panic or shutdown); skipping this cycle");
                return;
            }
        };

        // Re-send to the SAME server the op was consented to, addressed by the
        // persisted attester_id; the credit (or the refund) on settle still lands
        // on the op's account_id.
        for op in ops {
            let Some(direction) = OpDirection::parse(&op.direction) else {
                // The verb is not something to guess at: the two are opposites, so
                // re-driving this row the wrong way would consume the emeralds it
                // owes instead of paying them. Park it for a human. No refund
                // either — an unknown direction says nothing about what was
                // reserved.
                tracing::error!(op_id = %op.op_id, direction = %op.direction, state = %op.state,
                    "reconcile: op has an unknown direction — escalating to 'stuck' rather than \
                     guessing which verb re-drives it (invariant violated)");
                self.set_state(&op.op_id, "stuck").await;
                continue;
            };
            let Some(attester_id) = op.attester_id.as_deref().filter(|a| !a.is_empty()) else {
                // Every op this build writes carries its destination, and the v5
                // migration terminated the ones that predate the column — so this
                // is a broken invariant, not an expected state. Guessing a
                // destination would deliver a consume (or a payout) to a server the
                // user never approved.
                self.escalate_undeliverable(
                    &op,
                    direction,
                    "op has no attester_id — its destination is unknown and must NOT be guessed",
                )
                .await;
                continue;
            };
            match Uuid::parse_str(&op.mc_uuid) {
                // The resulting state is written to the ledger by `drive` itself;
                // reconciliation reports to nobody, so it is discarded here — but
                // an amount that cannot go on the wire is logged rather than
                // dropped, because every future pass will fail on it identically
                // and the op is only ever closed by the dead-letter pass.
                Ok(uuid) => {
                    if let Err(e) = self
                        .drive(
                            direction,
                            attester_id,
                            &uuid,
                            &op.op_id,
                            &op.idem_key,
                            op.amount,
                            "reconcile",
                        )
                        .await
                    {
                        tracing::error!(op_id = %op.op_id, direction = direction.as_str(), error = %e,
                            "reconcile: this op's amount cannot be expressed on the wire, so nothing \
                             was sent — it stays open for the dead-letter pass (which is safe: an op \
                             that never left consumed and granted nothing)");
                    }
                }
                Err(e) => {
                    // A persisted mc_uuid that fails to parse can never succeed;
                    // terminate it immediately instead of silently reselecting it
                    // every reconcile cycle.
                    tracing::error!(error = %e, op_id = %op.op_id, mc_uuid = %op.mc_uuid,
                        "reconcile: op has unparseable mc_uuid; terminating it");
                    self.escalate_undeliverable(&op, direction, "op has an unparseable mc_uuid")
                        .await;
                }
            }
        }
    }

    /// Terminate an op that can never be delivered, keeping BOTH the R008
    /// asymmetry and the refund obligation.
    ///
    /// A charge is failed when it provably never left (`pending`) and parked as
    /// `stuck` when it may have consumed (`sent`). A withdrawal splits the same
    /// way, except that failing one means giving the reserve back — so it goes
    /// through [`apply_withdraw_settlement`](withdraw::apply_withdraw_settlement),
    /// the only place that may do that, and NEVER through
    /// [`set_state`](Self::set_state), which would leave the eme debited and
    /// unreturned.
    async fn escalate_undeliverable(&self, op: &PendingOp, direction: OpDirection, why: &str) {
        let undelivered = op.state != "sent";
        tracing::error!(op_id = %op.op_id, state = %op.state, direction = direction.as_str(), why,
            "reconcile: escalating an undeliverable op instead of re-driving it");
        match (direction, undelivered) {
            (OpDirection::Charge, true) => self.set_state(&op.op_id, "failed").await,
            (OpDirection::Charge, false) => self.set_state(&op.op_id, "stuck").await,
            // Never delivered ⇒ nothing was granted ⇒ the reserve goes back.
            (OpDirection::Withdraw, true) => {
                self.apply_withdraw(
                    &op.op_id,
                    WithdrawSettlement::NotGranted,
                    WithdrawFrom::NeverSent,
                    "undeliverable_pending",
                )
                .await
            }
            // It reached the mod once; the payout may exist. Park it.
            (OpDirection::Withdraw, false) => {
                self.apply_withdraw(
                    &op.op_id,
                    WithdrawSettlement::Unknown,
                    WithdrawFrom::NotParked,
                    "undeliverable_sent",
                )
                .await
            }
        }
    }

    /// Update an op's state (best-effort; failures are logged, not fatal).
    ///
    /// Moves no money, which bounds what it may be used for: `'failed'` on a
    /// WITHDRAWAL would strand the reserve (debited, never refunded), so a
    /// withdrawal is only ever failed through
    /// [`apply_withdraw_settlement`](withdraw::apply_withdraw_settlement). The
    /// states that owe nobody anything — `'sent'` and `'stuck'` — are fine here in
    /// either direction.
    async fn set_state(&self, op_id: &str, state: &str) {
        let pool = self.pool.clone();
        let op_id = op_id.to_string();
        let state = state.to_string();
        let joined = tokio::task::spawn_blocking(move || match pool.get() {
            Ok(conn) => {
                if let Err(e) = conn.execute(
                    "UPDATE emerald_ops SET state = ?2, updated_unix_ms = ?3 \
                     WHERE op_id = ?1 AND state NOT IN ('settled','failed','stuck')",
                    params![op_id, state, now_ms()],
                ) {
                    tracing::warn!(error = %e, op_id = %op_id, state = %state, "set_state: update failed");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, op_id = %op_id, state = %state, "set_state: pool.get() failed");
            }
        })
        .await;
        if let Err(e) = joined {
            tracing::warn!(error = %e, "set_state: spawn_blocking join failed (panic or shutdown)");
        }
    }
}

/// Read a mod-reported amount off an ack **and convert it into the ledger's minor
/// units**.
///
/// The mod counts emeralds; everything past this function counts minor units. Both
/// settlers go through here — the charge one for `settled`, the withdraw one for
/// `granted` — which is what lets them compare the result against
/// `requested_amount` (already minor) in the same expression. Converting at the
/// call sites instead would be two chances to forget, on the two statements that
/// decide how much money moves.
///
/// Integer emerald counts encoded as floats (e.g. `100.0` by Gson/Java defaults)
/// make `as_i64()` return `None`, so fractionless floats are accepted too; a true
/// non-integer float is not (there is no half emerald).
///
/// `None` means "the mod did not tell us a usable amount" and is deliberately NOT
/// zero — the two callers cannot afford the same default. See each of them.
fn ack_amount(ack: &Value, field: &str) -> Option<i64> {
    ack.get(field)
        .and_then(|v| {
            v.as_i64().or_else(|| {
                v.as_f64().and_then(|f| {
                    if f.fract() == 0.0 {
                        Some(f as i64)
                    } else {
                        None
                    }
                })
            })
        })
        .and_then(crate::mc::to_minor)
}

/// Settle a charge ack into the ledger. `ack` = `{op_id, status, settled}`.
/// Idempotent: an op already in a terminal state is ignored, so a duplicate ack
/// never double-credits. Credits the balance ONLY on a successful consume, and
/// only on a `direction = 'charge'` op.
pub fn settle_ack(conn: &mut Connection, ack: &Value) -> rusqlite::Result<()> {
    let op_id = ack.get("op_id").and_then(Value::as_str).unwrap_or("");
    if op_id.is_empty() {
        tracing::warn!(ack = %ack, "settle: ack with missing/empty op_id (dropping)");
        return Ok(());
    }
    let status = ack.get("status").and_then(Value::as_str).unwrap_or("");
    // An unreadable `settled` becomes 0 here, which for a CHARGE is the safe
    // direction: it credits nothing rather than crediting an amount no emerald
    // paid for, and the warning below surfaces it. Do not copy this default into
    // the withdraw settler — there it would mean refunding a payout that the mod
    // said it made.
    let settled = ack_amount(ack, "settled").unwrap_or(0);

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
    let (account_id, requested, state, direction) = match row {
        Some(x) => x,
        None => {
            tx.commit()?;
            tracing::warn!(op_id, "settle: unknown op (dropping ack)");
            return Ok(());
        }
    };
    // Direction is checked against the LEDGER, not against whoever routed the ack
    // here: settling a withdrawal with this function would credit the account the
    // amount it just paid out, on top of the emeralds the mod granted.
    if direction != OpDirection::Charge.as_str() {
        tx.commit()?;
        tracing::error!(op_id, direction, status,
            "settle: refusing to apply a CHARGE settlement to an op that is not a charge");
        return Ok(());
    }
    // Terminal states are skipped for idempotency. NOTE (R008): `stuck` is
    // deliberately NOT terminal here — a `stuck` op (a `sent` op whose ack was
    // ambiguous) must remain settleable by a late ack, or its consumed emeralds
    // would be written off. Do NOT add `stuck` to this set.
    if state == "settled" || state == "failed" {
        tx.commit()?; // terminal — idempotent no-op
        return Ok(());
    }

    let now = now_ms();
    if status == "ok" || status == "duplicate" {
        let credited = settled.clamp(0, requested);
        if credited > 0 {
            wallet::credit_charge(&tx, &account_id, credited, now, CHARGE_LABEL)?;
        } else {
            // ok/duplicate ack with zero credit = 'settled' field was missing, non-integer,
            // or negative. Consumed emeralds go uncredited; surface for monitoring.
            tracing::warn!(
                op_id,
                status,
                requested,
                settled,
                "charge ack ok but credited 0 (missing/invalid 'settled'); no emeralds credited"
            );
        }
        tx.execute(
            "UPDATE emerald_ops SET state = 'settled', settled_amount = ?2, updated_unix_ms = ?3 WHERE op_id = ?1",
            params![op_id, credited, now],
        )?;
        tracing::info!(op_id, credited, "charge settled");
    } else {
        tx.execute(
            "UPDATE emerald_ops SET state = 'failed', settled_amount = 0, updated_unix_ms = ?2 WHERE op_id = ?1",
            params![op_id, now],
        )?;
        tracing::warn!(op_id, status, "charge failed (mod rejected)");
    }
    tx.commit()?;
    Ok(())
}

/// Read an `emerald_ops` row as `(owner_account_id, pollable view)`, if it
/// exists. The caller checks ownership (a session may only poll its own ops).
/// Works without the tunnel (pure ledger read).
pub fn op_view(conn: &Connection, op_id: &str) -> rusqlite::Result<Option<(String, Value)>> {
    conn.query_row(
        "SELECT op_id, account_id, direction, requested_amount, settled_amount, state, updated_unix_ms \
         FROM emerald_ops WHERE op_id = ?1",
        [op_id],
        |row| {
            let account_id: String = row.get(1)?;
            let view = json!({
                "op_id": row.get::<_, String>(0)?,
                "direction": row.get::<_, String>(2)?,
                "requested_amount": row.get::<_, i64>(3)?,
                "settled_amount": row.get::<_, Option<i64>>(4)?,
                "state": row.get::<_, String>(5)?,
                "updated_ms": row.get::<_, i64>(6)?,
            });
            Ok((account_id, view))
        },
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn charge_scope_isolates_accounts() {
        // Two accounts reusing one idem_key must not reach each other's op.
        assert_ne!(charge_scope("acct-a"), charge_scope("acct-b"));
        assert_eq!(charge_scope("acct-a"), charge_scope("acct-a"));
        // …and a charge scope can never collide with the send/pay scopes that
        // share the (idem_key, scope) primary key.
        assert_ne!(charge_scope("acct-a"), "send");
        assert_ne!(charge_scope("acct-a"), "pay");
        assert_ne!(charge_scope("acct-a"), "charge");
    }

    #[test]
    fn a_mod_reported_amount_arrives_in_the_ledgers_units() {
        // The mod says "10 emeralds"; the ledger has to hear 1,000 minor units,
        // because the very next thing both settlers do is compare it against
        // `requested_amount`. Reading it raw would settle a charge at a hundredth
        // of what was asked and park every withdrawal as a shortfall.
        assert_eq!(
            ack_amount(&json!({ "settled": 10 }), "settled"),
            Some(1_000)
        );
        assert_eq!(
            ack_amount(&json!({ "granted": 10 }), "granted"),
            Some(1_000)
        );
        // Gson writes integer counts as floats; still emeralds, still converted.
        assert_eq!(ack_amount(&json!({ "settled": 10.0 }), "settled"), Some(1_000));
        // Half an emerald is not a count of emeralds, converted or otherwise.
        assert_eq!(ack_amount(&json!({ "settled": 9.5 }), "settled"), None);
        assert_eq!(ack_amount(&json!({ "settled": "10" }), "settled"), None);
        assert_eq!(ack_amount(&json!({}), "settled"), None);
        // Each direction still reads only its own field.
        assert_eq!(ack_amount(&json!({ "granted": 10 }), "settled"), None);
    }

    #[test]
    fn the_charge_screen_is_shown_money_not_items() {
        // 9 emeralds per block, then into minor units — the block rate is an
        // in-world fact and the ×100 is the ledger's, and they compose in that
        // order.
        assert_eq!(chargeable_minor(0, 0), 0);
        assert_eq!(chargeable_minor(7, 0), 700);
        assert_eq!(chargeable_minor(0, 1), 900);
        assert_eq!(chargeable_minor(5, 2), (5 + 18) * 100);
        // A mod reporting an impossible inventory produces a large number, never a
        // negative one: this is what a charge screen displays, and a wrapped total
        // would show the player a debt.
        assert_eq!(chargeable_minor(i64::MAX, i64::MAX), i64::MAX);
    }

    #[tokio::test]
    async fn reconcile_never_redrives_an_op_without_an_attester() {
        let (pool, coord) = coordinator();
        insert_op(&pool, "op-null-pending", None, "pending");
        insert_op(&pool, "op-null-sent", None, "sent");
        insert_op(&pool, "op-addressed", Some("mc1"), "pending");

        coord.reconcile().await;

        // No destination ⇒ escalated, never delivered to a guessed server. The
        // asymmetry is the R008 one: 'pending' consumed nothing so it can be
        // failed, 'sent' may have consumed so it needs a human.
        assert_eq!(state_of(&pool, "op-null-pending"), "failed");
        assert_eq!(state_of(&pool, "op-null-sent"), "stuck");
        // An addressed op WAS driven — and with the tunnel down that reports
        // `NotSent`, which leaves it exactly where it was for the next cycle.
        assert_eq!(state_of(&pool, "op-addressed"), "pending");
    }

    #[tokio::test]
    async fn a_replay_is_scoped_to_its_own_account() {
        let (pool, coord) = coordinator();
        {
            let conn = pool.get().unwrap();
            db::idem_put(
                &conn,
                "k1",
                &charge_scope("acct-a"),
                &json!({ "ok": true, "op_id": "op-1", "state": "pending" }).to_string(),
            )
            .unwrap();
        }

        let mine = coord.replay_charge("acct-a", "k1").await.unwrap();
        let mine = mine.expect("acct-a's own charge replays");
        assert_eq!(mine["op_id"], "op-1");
        assert_eq!(mine["duplicate"], json!(true));

        // Another account presenting the same key finds nothing — so it cannot
        // pick up acct-a's op_id, and (having no replay) it must produce a fresh
        // assertion before anything is consumed.
        assert!(coord.replay_charge("acct-b", "k1").await.unwrap().is_none());
        assert!(coord.replay_charge("acct-a", "k2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_corrupt_replay_record_is_not_reported_as_success() {
        let (pool, coord) = coordinator();
        {
            let conn = pool.get().unwrap();
            db::idem_put(&conn, "k1", &charge_scope("acct-a"), "{not json").unwrap();
            db::idem_put(&conn, "k2", &charge_scope("acct-a"), "{\"ok\":true}").unwrap();
        }
        for key in ["k1", "k2"] {
            let v = coord
                .replay_charge("acct-a", key)
                .await
                .unwrap()
                .expect("a record exists");
            assert_eq!(v["ok"], json!(false), "{key}");
            assert_eq!(v["error"], "charge_failed", "{key}");
        }
    }

    #[test]
    fn neither_direction_can_settle_the_others_op() {
        let (pool, _coord) = coordinator();
        fund(&pool, 0);
        insert_withdraw(&pool, "op-w", "sent", 1_000, false);
        insert_op(&pool, "op-c", Some("mc1"), "sent");

        // A charge settlement on a withdrawal would CREDIT the account the amount
        // it just paid out, on top of the emeralds the mod granted.
        charge_ack(
            &pool,
            json!({ "op_id": "op-w", "status": "ok", "settled": 10 }),
        );
        // …and a withdraw settlement on a charge would refund emeralds that were
        // consumed, not paid.
        withdraw_ack(
            &pool,
            json!({ "op_id": "op-c", "status": "player_offline", "granted": 0 }),
        );

        assert_eq!(state_of(&pool, "op-w"), "sent");
        assert_eq!(state_of(&pool, "op-c"), "sent");
        assert_eq!(balance_of(&pool), 0);
        assert!(txns(&pool).is_empty());
    }
}
