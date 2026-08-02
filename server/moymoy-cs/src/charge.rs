//! Emerald-charge coordinator: the bridge between the in-world mod (truth of
//! emerald consumption) and the wallet (truth of balance), reconciled through the
//! `emerald_ops` ledger.
//!
//! Consistency model (DEV.md): consume-first with at-least-once delivery + an
//! op-keyed idempotent settlement. The balance is credited ONLY when the mod's
//! settlement ack says so (state → settled), never on send — so a lost ack never
//! mints eme that no emerald paid for, and a duplicate ack never double-credits.
//! A reconciliation pass re-sends non-terminal ops so a dropped request/ack still
//! eventually settles (the mod is op-idempotent and re-acks).
//!
//! Since the move to HTTP in MNN (MochiOS DEV.md §7.3.10) the ack arrives as the
//! **response to the charge request**, so the common case now settles inside
//! [`ChargeCoordinator::begin_charge`] instead of minutes later on an inbound
//! frame. The ledger is unchanged and still load-bearing: an exchange that fails
//! after the mod consumed is [`ChargeOutcome::Ambiguous`], and only
//! reconciliation — driving the same `op_id` until the mod re-acks — can close it.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::db::{self, now_ms, Pool};
use crate::error::ApiError;
use crate::mc::{ChargeOutcome, McLink};
use crate::wallet;

/// Charge-txn label so a real emerald charge is distinguishable in the ledger.
const CHARGE_LABEL: &str = "インベントリのエメラルド";

/// A non-terminal op older than this is dead-lettered by reconciliation (R008):
/// a never-delivered `pending` op becomes `failed` (no emeralds consumed), while
/// a `sent` op (consumption ambiguous) becomes `stuck` for manual review — never
/// silently failed, so consumed emeralds are not written off.
const DEAD_LETTER_MS: i64 = 24 * 60 * 60 * 1000;

/// Internal outcome of the begin-charge transaction.
enum BeginCharge {
    /// The character is already claimed by a *different* MoyMoy account (R007).
    Claimed,
    /// A prior op exists for this idem_key — replay it.
    Existing(String),
    /// A fresh op was created (and the character linked).
    Fresh(String),
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

    /// Query a Minecraft character's chargeable inventory via the mod, keyed by
    /// `mc_uuid` (the current gameUuid — distinct from the MoyMoy account_id since
    /// v2). Only reached when `can_charge()` is true.
    pub async fn query_inventory(&self, mc_uuid: &str) -> Result<Inventory, ApiError> {
        let uuid = Uuid::parse_str(mc_uuid)
            .map_err(|_| ApiError::bad_request("mc_uuid is not a UUID"))?;
        match self.mc.query_inventory(&uuid).await {
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

    /// Begin an emerald charge: record a pending `emerald_ops` row (idempotent on
    /// `idem_key`), auto-link the Minecraft character to the MoyMoy account, ask
    /// the mod to consume, and return a pollable op (`GET /wallet/op`). The
    /// balance is credited from the mod's ack — to `account_id` (the MoyMoy
    /// account), while consumption is routed by `mc_uuid` (the character).
    ///
    /// The ack is normally the charge request's own HTTP response, so the op this
    /// returns is usually already `settled`. It stays pollable because the other
    /// outcomes (not routable, tunnel down, ambiguous) settle later, on a
    /// reconciliation pass.
    pub async fn begin_charge(
        &self,
        idem_key: &str,
        account_id: &str,
        mc_uuid: &str,
        mcid: Option<&str>,
        amount: i64,
    ) -> Result<Value, ApiError> {
        if amount <= 0 || amount > wallet::MAX_AMOUNT {
            return Ok(json!({ "ok": false, "error": "bad_amount" }));
        }

        // 1. Create (or replay) the op + link the character in one transaction.
        let pool = self.pool.clone();
        let ik = idem_key.to_string();
        let aid = account_id.to_string();
        let muuid = mc_uuid.to_string();
        let mcid_owned = mcid.map(str::to_string);
        let outcome = tokio::task::spawn_blocking(move || -> Result<BeginCharge, ApiError> {
            let mut conn = pool.get()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(prev) = db::idem_get(&tx, &ik, "charge")? {
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
            // R007: a character belongs to exactly one account. Reject a charge
            // from a character already claimed by someone else — never consume
            // their emeralds. (Unclaimed or self-owned proceeds and links below.)
            if let Some(owner) = crate::identity::mc_link_owner(&tx, &muuid)? {
                if owner != aid {
                    return Ok(BeginCharge::Claimed);
                }
            }
            let op_id = Uuid::new_v4().to_string();
            let now = now_ms();
            tx.execute(
                "INSERT INTO emerald_ops \
                   (op_id, idem_key, account_id, mc_uuid, direction, requested_amount, settled_amount, state, created_unix_ms, updated_unix_ms) \
                 VALUES (?1, ?2, ?3, ?4, 'charge', ?5, NULL, 'pending', ?6, ?6)",
                params![op_id, ik, aid, muuid, amount, now],
            )?;
            // Auto-link the character to this account (verified: the gameUuid is
            // runtime-attested in-world). The v3 UNIQUE(mc_uuid) backstops a race.
            crate::identity::link_mc(&tx, &aid, &muuid, mcid_owned.as_deref())?;
            let resp = json!({ "ok": true, "op_id": op_id, "state": "pending" });
            db::idem_put(&tx, &ik, "charge", &resp.to_string())?;
            tx.commit()?;
            Ok(BeginCharge::Fresh(op_id))
        })
        .await??;

        let (op_id, fresh) = match outcome {
            BeginCharge::Claimed => {
                return Ok(json!({ "ok": false, "error": "character_claimed" }))
            }
            BeginCharge::Existing(op) => (op, false),
            BeginCharge::Fresh(op) => (op, true),
        };

        if op_id.is_empty() {
            return Ok(json!({ "ok": false, "error": "charge_failed" }));
        }
        if !fresh {
            return Ok(json!({ "ok": true, "op_id": op_id, "state": "pending", "duplicate": true }));
        }

        // 2. Ask the mod to consume the emeralds (auto-routed to the live server
        //    by the character's mc_uuid) and settle its ack.
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
            .drive(&uuid, &op_id, idem_key, amount, "begin_charge")
            .await
            .unwrap_or_else(|| "pending".to_string());
        Ok(json!({ "ok": true, "op_id": op_id, "state": state }))
    }

    /// Send one charge attempt for `op_id` and fold its outcome into the ledger.
    /// Shared by the primary path and reconciliation so a retry can never take a
    /// different route than the original — the mod is `op_id`-idempotent, so both
    /// are the same operation.
    ///
    /// Returns the op's new state when this attempt moved it, `None` when it left
    /// the op untouched. The transitions encode **whether emeralds may have been
    /// consumed**:
    ///
    /// * acked → settled / failed by [`settle_ack`] (the only path that credits)
    /// * not routable / nothing sent → untouched (nothing consumed, so a stale
    ///   `pending` is safe for the dead-letter pass to fail)
    /// * ambiguous → `sent`, which the dead-letter pass escalates to `stuck` for
    ///   manual review rather than writing consumed emeralds off
    async fn drive(
        &self,
        uuid: &Uuid,
        op_id: &str,
        idem_key: &str,
        amount: i64,
        origin: &str,
    ) -> Option<String> {
        match self.mc.send_charge(uuid, op_id, idem_key, amount).await {
            ChargeOutcome::Acked(ack) => Some(self.settle_now(op_id, ack).await),
            ChargeOutcome::PlayerOffline | ChargeOutcome::NotSent => None,
            ChargeOutcome::Ambiguous(msg) => {
                tracing::warn!(op_id, origin, error = %msg,
                    "charge exchange failed with consumption UNKNOWN; marking 'sent' so reconciliation re-drives it");
                self.set_state(op_id, "sent").await;
                Some("sent".to_string())
            }
        }
    }

    /// Apply the mod's ack to the ledger and read back the op's resulting state,
    /// in ONE blocking hop so the state reported is the one this ack produced.
    async fn settle_now(&self, op_id: &str, ack: Value) -> String {
        let pool = self.pool.clone();
        let op = op_id.to_string();
        let joined = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get()?;
            settle_ack(&mut conn, &ack)?;
            Ok::<Option<String>, ApiError>(
                op_view(&conn, &op)?.and_then(|(_, v)| {
                    v.get("state").and_then(Value::as_str).map(str::to_string)
                }),
            )
        })
        .await;

        let failure = match joined {
            Ok(Ok(Some(state))) => return state,
            Ok(Ok(None)) => "the op is not in the ledger".to_string(),
            Ok(Err(e)) => e.to_string(),
            Err(e) => format!("spawn_blocking join failed (panic or shutdown): {e}"),
        };
        // The mod ANSWERED, so emeralds may already be gone and we merely failed
        // to record it. Park the op in 'sent' — the state that means "consumption
        // unknown" — so reconciliation re-drives it (the mod re-acks `duplicate`
        // with the same settled amount) and, if it never closes, the dead-letter
        // pass escalates it to 'stuck' for review instead of failing it as
        // never-delivered and writing consumed emeralds off.
        tracing::error!(op_id, error = %failure,
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
        let ops: Vec<(String, String, String, i64)> = match tokio::task::spawn_blocking(move || {
            let conn = match pool.get() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "reconcile: pool.get failed");
                    return Vec::new();
                }
            };
            // R008: dead-letter ops too old to keep retrying. A never-delivered
            // `pending` op (no emeralds consumed) is safe to fail; a `sent` op
            // (consumption ambiguous) goes to `stuck` for manual review — never
            // auto-failed, so consumed emeralds aren't written off. A late ack can
            // still settle a `stuck` op (settle_ack doesn't skip it).
            let cutoff = now_ms() - DEAD_LETTER_MS;
            match conn.execute(
                "UPDATE emerald_ops SET state = 'failed', updated_unix_ms = ?2 \
                 WHERE state = 'pending' AND created_unix_ms < ?1",
                params![cutoff, now_ms()],
            ) {
                Ok(n) if n > 0 => {
                    tracing::warn!(count = n, "reconcile: dead-lettered stale pending ops -> failed (never delivered)")
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "reconcile: dead-letter pending failed"),
            }
            match conn.execute(
                "UPDATE emerald_ops SET state = 'stuck', updated_unix_ms = ?2 \
                 WHERE state = 'sent' AND created_unix_ms < ?1",
                params![cutoff, now_ms()],
            ) {
                Ok(n) if n > 0 => {
                    tracing::error!(count = n, "reconcile: dead-lettered stale sent ops -> stuck (consumption ambiguous; needs manual review)")
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "reconcile: dead-letter sent failed"),
            }
            let mut stmt = match conn.prepare(
                "SELECT op_id, idem_key, mc_uuid, requested_amount FROM emerald_ops \
                 WHERE state IN ('pending','sent') ORDER BY created_unix_ms ASC LIMIT 50",
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "reconcile: prepare failed");
                    return Vec::new();
                }
            };
            match stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
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

        // Re-send routed by the character's mc_uuid; the credit on settle still
        // lands on the op's account_id (see settle_ack).
        for (op_id, idem_key, mc_uuid, amount) in ops {
            match Uuid::parse_str(&mc_uuid) {
                // The resulting state is written to the ledger by `drive` itself;
                // reconciliation reports to nobody, so it is discarded here.
                Ok(uuid) => {
                    let _ = self
                        .drive(&uuid, &op_id, &idem_key, amount, "reconcile")
                        .await;
                }
                Err(e) => {
                    // A persisted mc_uuid that fails to parse can never succeed;
                    // terminate it immediately (mirrors the primary charge path)
                    // instead of silently reselecting it every reconcile cycle.
                    tracing::error!(error = %e, op_id = %op_id, mc_uuid = %mc_uuid,
                        "reconcile: op has unparseable mc_uuid; marking failed");
                    self.set_state(&op_id, "failed").await;
                }
            }
        }
    }

    /// Update an op's state (best-effort; failures are logged, not fatal).
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

/// Settle a mod ack into the ledger. `ack` = `{op_id, status, settled}`.
/// Idempotent: an op already in a terminal state is ignored, so a duplicate ack
/// never double-credits. Credits the balance ONLY on a successful consume.
pub fn settle_ack(conn: &mut Connection, ack: &Value) -> rusqlite::Result<()> {
    let op_id = ack.get("op_id").and_then(Value::as_str).unwrap_or("");
    if op_id.is_empty() {
        tracing::warn!(ack = %ack, "settle: ack with missing/empty op_id (dropping)");
        return Ok(());
    }
    let status = ack.get("status").and_then(Value::as_str).unwrap_or("");
    // `settled` arrives from an external mod over JSON. Integer emerald counts
    // encoded as floats (e.g. 100.0 by Gson/Java defaults) make as_i64() return
    // None, which would unwrap_or(0) and write off consumed emeralds (asset loss).
    // Accept integers or fractionless floats; reject true non-integer floats.
    let settled = ack
        .get("settled")
        .and_then(|v| {
            v.as_i64().or_else(|| {
                v.as_f64()
                    .and_then(|f| if f.fract() == 0.0 { Some(f as i64) } else { None })
            })
        })
        .unwrap_or(0);

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row = tx
        .query_row(
            "SELECT account_id, requested_amount, state FROM emerald_ops WHERE op_id = ?1",
            [op_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let (account_id, requested, state) = match row {
        Some(x) => x,
        None => {
            tx.commit()?;
            tracing::warn!(op_id, "settle: unknown op (dropping ack)");
            return Ok(());
        }
    };
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
                op_id, status, requested, settled,
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
