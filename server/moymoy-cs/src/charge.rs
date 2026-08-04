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

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::db::{self, now_ms, Pool};
use crate::error::ApiError;
use crate::mc::{ChargeOutcome, McLink};
use crate::wallet;

/// Charge-txn label so a real emerald charge is distinguishable in the ledger.
const CHARGE_LABEL: &str = "インベントリのエメラルド";

/// Withdraw-txn label on the debit that reserves the payout.
const WITHDRAW_LABEL: &str = "エメラルドで受け取り";

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

/// Internal outcome of the begin-withdraw transaction. Unlike [`BeginCharge`] it
/// carries the refusals too: a withdrawal is refused by the same transaction that
/// would have reserved the eme, so the answer comes back from in there.
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

/// The idempotency scope for a withdrawal: `withdraw:<account_id>`.
///
/// Account-scoped for the same reason [`charge_scope`] is, and a DIFFERENT prefix
/// so one account's own key cannot cross the two: replaying a charge's key
/// against `/wallet/withdraw` must not hand back the charge's op (nor stop the
/// withdrawal being created), because the two move value in opposite directions.
pub fn withdraw_scope(account_id: &str) -> String {
    format!("withdraw:{account_id}")
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

/// Player inventory snapshot for the charge screen (9 eme = 1 block).
///
/// `reachable`/`online` keep the three real outcomes distinct instead of
/// collapsing them to "0 emeralds": `reachable=false` ⇒ the mod never answered
/// (offline / server doesn't host moymoy / MC connector down); `online=false` ⇒
/// the mod answered but the UUID isn't a live player there (a UUID mismatch shows
/// up here, NOT as a genuine zero balance).
#[derive(Debug)]
pub struct Inventory {
    pub reachable: bool,
    pub online: bool,
    pub emeralds: i64,
    pub blocks: i64,
    pub chargeable: i64,
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
                chargeable: emeralds + blocks * 9,
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
            .await
            .unwrap_or_else(|| "pending".to_string());
        Ok(json!({ "ok": true, "op_id": op_id, "state": state }))
    }

    /// Begin a withdrawal: debit `amount` エメ, record a pending `emerald_ops` row
    /// (idempotent on `(account_id, idem_key)`), ask the mod on `attester_id` to
    /// grant the emeralds to `mc_uuid`, and return a pollable op
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
            .await
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
    ) -> Option<String> {
        let outcome = match direction {
            OpDirection::Charge => {
                self.mc
                    .send_charge(attester_id, uuid, op_id, idem_key, amount)
                    .await
            }
            OpDirection::Withdraw => {
                self.mc
                    .send_withdraw(attester_id, uuid, op_id, idem_key, amount)
                    .await
            }
        };
        match outcome {
            ChargeOutcome::Acked(ack) => Some(self.settle_now(direction, op_id, ack).await),
            ChargeOutcome::ServerUnreachable | ChargeOutcome::NotSent => None,
            ChargeOutcome::Ambiguous(msg) => {
                tracing::warn!(op_id, origin, direction = direction.as_str(), error = %msg,
                    "emerald op exchange failed with its in-world effect UNKNOWN; marking 'sent' so reconciliation re-drives it");
                self.set_state(op_id, "sent").await;
                Some("sent".to_string())
            }
        }
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
                // reconciliation reports to nobody, so it is discarded here.
                Ok(uuid) => {
                    let _ = self
                        .drive(
                            direction,
                            attester_id,
                            &uuid,
                            &op.op_id,
                            &op.idem_key,
                            op.amount,
                            "reconcile",
                        )
                        .await;
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
    /// through [`apply_withdraw_settlement`], the only place that may do that,
    /// and NEVER through [`set_state`](Self::set_state), which would leave the eme
    /// debited and unreturned.
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

    /// Run one withdraw settlement on the blocking pool (best-effort; failures are
    /// logged, not fatal — the op stays where it is and reconciliation retries).
    async fn apply_withdraw(
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

    /// Update an op's state (best-effort; failures are logged, not fatal).
    ///
    /// Moves no money, which bounds what it may be used for: `'failed'` on a
    /// WITHDRAWAL would strand the reserve (debited, never refunded), so a
    /// withdrawal is only ever failed through [`apply_withdraw_settlement`]. The
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

/// Read a mod-reported amount off an ack.
///
/// Integer emerald counts encoded as floats (e.g. `100.0` by Gson/Java defaults)
/// make `as_i64()` return `None`, so fractionless floats are accepted too; a true
/// non-integer float is not (it is not a number of emeralds).
///
/// `None` means "the mod did not tell us a usable amount" and is deliberately NOT
/// zero — the two callers cannot afford the same default. See each of them.
fn ack_amount(ack: &Value, field: &str) -> Option<i64> {
    ack.get(field).and_then(|v| {
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

/// What a withdrawal's outcome turned out to be. Three values, not two, because
/// "we don't know" is a real answer here and rounding it either way loses
/// somebody's assets.
#[derive(Clone, Copy, Debug)]
enum WithdrawSettlement {
    /// The mod reported granting this many エメ worth of emeralds. Any shortfall
    /// against the reserve is refunded, because the ack proves it was not paid.
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
enum WithdrawFrom {
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
fn apply_withdraw_settlement(
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
fn dead_letter_withdrawals(conn: &mut Connection, cutoff: i64) {
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
    use super::*;
    use crate::db::Pool;

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

    fn coordinator() -> (Pool, ChargeCoordinator) {
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

    fn insert_op(pool: &Pool, op_id: &str, attester: Option<&str>, state: &str) {
        insert_full_op(pool, op_id, attester, "charge", state, 10, now_ms());
    }

    /// An `emerald_ops` row exactly as written, for the cases a test needs to
    /// control the direction, the amount or the age.
    fn insert_full_op(
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
    fn insert_withdraw(pool: &Pool, op_id: &str, state: &str, amount: i64, aged: bool) {
        let created = if aged {
            now_ms() - DEAD_LETTER_MS - 1_000
        } else {
            now_ms()
        };
        insert_full_op(
            pool,
            op_id,
            Some("mc1"),
            "withdraw",
            state,
            amount,
            created,
        );
    }

    fn state_of(pool: &Pool, op_id: &str) -> String {
        pool.get()
            .unwrap()
            .query_row(
                "SELECT state FROM emerald_ops WHERE op_id = ?1",
                [op_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn settled_of(pool: &Pool, op_id: &str) -> Option<i64> {
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
    fn fund(pool: &Pool, amount: i64) {
        pool.get()
            .unwrap()
            .execute(
                "UPDATE accounts SET balance = ?1 WHERE account_id = 'acct-a'",
                [amount],
            )
            .unwrap();
    }

    fn balance_of(pool: &Pool) -> i64 {
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
    fn txns(pool: &Pool) -> Vec<(String, i64)> {
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
    fn withdraw_ack(pool: &Pool, ack: Value) {
        let mut conn = pool.get().unwrap();
        settle_withdraw_ack(&mut conn, &ack).expect("the settlement applies");
    }

    fn charge_ack(pool: &Pool, ack: Value) {
        let mut conn = pool.get().unwrap();
        settle_ack(&mut conn, &ack).expect("the settlement applies");
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

    // ── withdraw ─────────────────────────────────────────────────────────────

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
        fund(&pool, 100);

        let v = coord
            .begin_withdraw("k1", "acct-a", UUID, "mc1", 40)
            .await
            .unwrap();
        assert_eq!(v["ok"], json!(true));
        // The tunnel is down, so nothing was sent and the op waits for
        // reconciliation — with the eme already debited, which is the point.
        assert_eq!(v["state"], "pending");
        assert_eq!(balance_of(&pool), 60);
        assert_eq!(txns(&pool), vec![("withdraw".to_string(), -40)]);
        let op_id = v["op_id"].as_str().unwrap().to_string();
        assert_eq!(state_of(&pool, &op_id), "pending");

        // A retry of the same key replays that op instead of debiting again.
        let again = coord
            .begin_withdraw("k1", "acct-a", UUID, "mc1", 40)
            .await
            .unwrap();
        assert_eq!(again["op_id"], json!(op_id));
        assert_eq!(again["duplicate"], json!(true));
        assert_eq!(balance_of(&pool), 60);
        assert_eq!(txns(&pool).len(), 1);
    }

    #[tokio::test]
    async fn a_withdrawal_that_cannot_proceed_writes_nothing_at_all() {
        let (pool, coord) = coordinator();
        fund(&pool, 100);

        let short = coord
            .begin_withdraw("k-short", "acct-a", UUID, "mc1", 101)
            .await
            .unwrap();
        assert_eq!(short["ok"], json!(false));
        assert_eq!(short["error"], "insufficient");
        assert_eq!(short["balance"], json!(100));

        // A bad destination is refused BEFORE the debit, unlike a charge (which
        // creates the op first) — nothing is owed back because nothing was taken.
        let bad_uuid = coord
            .begin_withdraw("k-uuid", "acct-a", "not-a-uuid", "mc1", 10)
            .await
            .unwrap();
        assert_eq!(bad_uuid["error"], "bad_uuid");

        let over = coord
            .begin_withdraw("k-max", "acct-a", UUID, "mc1", wallet::MAX_WITHDRAW_PER_OP + 1)
            .await
            .unwrap();
        assert_eq!(over["error"], "bad_amount");

        assert_eq!(balance_of(&pool), 100);
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
        insert_withdraw(&pool, "op-w", "sent", 10, false);

        withdraw_ack(
            &pool,
            json!({ "op_id": "op-w", "status": "ok", "granted": 10 }),
        );

        assert_eq!(state_of(&pool, "op-w"), "settled");
        assert_eq!(settled_of(&pool, "op-w"), Some(10));
        // The debit stands: the player has the emeralds.
        assert_eq!(balance_of(&pool), 0);
        assert!(txns(&pool).is_empty());
    }

    #[test]
    fn a_partial_grant_refunds_only_the_shortfall() {
        let (pool, _coord) = coordinator();
        fund(&pool, 0);
        insert_withdraw(&pool, "op-w", "sent", 10, false);

        // Shouldn't happen, but the ack states exactly what was paid, so the rest
        // is provably unpaid and goes back.
        withdraw_ack(
            &pool,
            json!({ "op_id": "op-w", "status": "duplicate", "granted": 4 }),
        );

        assert_eq!(state_of(&pool, "op-w"), "settled");
        assert_eq!(settled_of(&pool, "op-w"), Some(4));
        assert_eq!(balance_of(&pool), 6);
        assert_eq!(txns(&pool), vec![("withdraw".to_string(), 6)]);
    }

    #[test]
    fn the_same_failure_ack_refunds_exactly_once() {
        // THE test. At-least-once delivery means the mod's answer can arrive more
        // than once, and reconciliation re-drives ops on a timer; a refund that is
        // not idempotent mints eme on every repeat.
        let (pool, _coord) = coordinator();
        fund(&pool, 0);
        insert_withdraw(&pool, "op-w", "sent", 10, false);

        let ack = json!({ "op_id": "op-w", "status": "player_offline", "granted": 0 });
        for _ in 0..5 {
            withdraw_ack(&pool, ack.clone());
        }

        assert_eq!(state_of(&pool, "op-w"), "failed");
        assert_eq!(settled_of(&pool, "op-w"), Some(0));
        assert_eq!(balance_of(&pool), 10);
        assert_eq!(txns(&pool), vec![("withdraw".to_string(), 10)]);
    }

    #[test]
    fn an_unprovable_ack_parks_the_op_and_never_refunds() {
        let (pool, _coord) = coordinator();
        fund(&pool, 0);
        insert_withdraw(&pool, "op-unknown", "sent", 10, false);
        insert_withdraw(&pool, "op-internal", "sent", 10, false);
        insert_withdraw(&pool, "op-novel", "sent", 10, false);

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
        insert_withdraw(&pool, "op-missing", "sent", 10, false);
        insert_withdraw(&pool, "op-fractional", "sent", 10, false);
        insert_withdraw(&pool, "op-float", "sent", 10, false);

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
        assert_eq!(settled_of(&pool, "op-float"), Some(10));
        assert_eq!(balance_of(&pool), 0);
        assert!(txns(&pool).is_empty());
    }

    #[test]
    fn a_parked_withdrawal_can_still_settle_but_can_never_auto_refund() {
        let (pool, _coord) = coordinator();
        fund(&pool, 0);
        insert_withdraw(&pool, "op-w", "stuck", 10, false);

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

    #[test]
    fn neither_direction_can_settle_the_others_op() {
        let (pool, _coord) = coordinator();
        fund(&pool, 0);
        insert_withdraw(&pool, "op-w", "sent", 10, false);
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

    #[tokio::test]
    async fn the_dead_letter_pass_refunds_undelivered_withdrawals_once_and_parks_delivered_ones() {
        let (pool, coord) = coordinator();
        fund(&pool, 0); // 10 + 7 エメ are reserved by the two withdrawals below
        insert_withdraw(&pool, "w-pending", "pending", 10, true);
        insert_withdraw(&pool, "w-sent", "sent", 7, true);
        insert_full_op(
            &pool,
            "c-pending",
            Some("mc1"),
            "charge",
            "pending",
            10,
            now_ms() - DEAD_LETTER_MS - 1_000,
        );
        insert_full_op(
            &pool,
            "c-sent",
            Some("mc1"),
            "charge",
            "sent",
            10,
            now_ms() - DEAD_LETTER_MS - 1_000,
        );
        // A fresh withdrawal is not overdue and must be left alone.
        insert_withdraw(&pool, "w-fresh", "pending", 5, false);

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

        assert_eq!(balance_of(&pool), 10);
        assert_eq!(txns(&pool), vec![("withdraw".to_string(), 10)]);
    }

    #[tokio::test]
    async fn an_undeliverable_withdrawal_is_refunded_rather_than_stranded() {
        // `set_state` cannot be used to fail a withdrawal — it moves no money, so
        // the eme would stay debited with nothing left to refund it. This is the
        // path where that would be easiest to get wrong.
        let (pool, coord) = coordinator();
        fund(&pool, 0);
        insert_full_op(&pool, "w-null-pending", None, "withdraw", "pending", 10, now_ms());
        insert_full_op(&pool, "w-null-sent", None, "withdraw", "sent", 7, now_ms());
        // A direction nothing knows how to drive: escalated, never guessed at, and
        // never refunded (an unknown direction says nothing about what was
        // reserved).
        insert_full_op(&pool, "w-alien", Some("mc1"), "transmute", "pending", 3, now_ms());

        coord.reconcile().await;

        assert_eq!(state_of(&pool, "w-null-pending"), "failed");
        assert_eq!(state_of(&pool, "w-null-sent"), "stuck");
        assert_eq!(state_of(&pool, "w-alien"), "stuck");
        assert_eq!(balance_of(&pool), 10);
        assert_eq!(txns(&pool), vec![("withdraw".to_string(), 10)]);
    }
}
