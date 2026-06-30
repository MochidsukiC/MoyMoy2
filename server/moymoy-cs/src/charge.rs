//! Emerald-charge coordinator: the bridge between the in-world mod (truth of
//! emerald consumption) and the wallet (truth of balance), reconciled through the
//! `emerald_ops` ledger.
//!
//! Consistency model (DEV.md): consume-first with at-least-once delivery + an
//! op-keyed idempotent settlement. The balance is credited ONLY when the mod's
//! settlement ack arrives (state → settled), never on send — so a lost ack never
//! mints eme that no emerald paid for, and a duplicate ack never double-credits.
//! A reconciliation pass re-sends non-terminal ops so a dropped command/ack still
//! eventually settles (the mod is op-idempotent and re-acks).

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::command_bus::{CommandBus, SendOutcome};
use crate::db::{self, now_ms, Pool};
use crate::error::ApiError;
use crate::wallet;

/// Charge-txn label so a real emerald charge is distinguishable in the ledger.
const CHARGE_LABEL: &str = "インベントリのエメラルド";

/// Player inventory snapshot for the charge screen (9 eme = 1 block).
#[derive(Debug)]
pub struct Inventory {
    pub emeralds: i64,
    pub blocks: i64,
    pub chargeable: i64,
}

/// Drives emerald charges over the command bus. Holds the SQLite pool (for the
/// `emerald_ops` ledger) and the optional command bus.
pub struct ChargeCoordinator {
    pool: Pool,
    bus: Option<CommandBus>,
}

impl ChargeCoordinator {
    pub fn new(pool: Pool, bus: Option<CommandBus>) -> Self {
        ChargeCoordinator { pool, bus }
    }

    /// Whether emerald charging is available (the command bus is connected).
    pub fn can_charge(&self) -> bool {
        self.bus.is_some()
    }

    /// Query a Minecraft character's chargeable inventory via the mod, keyed by
    /// `mc_uuid` (the current gameUuid — distinct from the MoyMoy account_id since
    /// v2). Only reached when `can_charge()` is true.
    pub async fn query_inventory(&self, mc_uuid: &str) -> Result<Inventory, ApiError> {
        let bus = self
            .bus
            .as_ref()
            .ok_or_else(|| ApiError::internal("charge unavailable"))?;
        let uuid = Uuid::parse_str(mc_uuid)
            .map_err(|_| ApiError::bad_request("mc_uuid is not a UUID"))?;
        match bus.query_inventory(&uuid).await {
            Some((emeralds, blocks)) => Ok(Inventory {
                emeralds,
                blocks,
                chargeable: emeralds + blocks * 9,
            }),
            // Player offline / mod didn't answer — report empty (frontend shows 0).
            None => Ok(Inventory {
                emeralds: 0,
                blocks: 0,
                chargeable: 0,
            }),
        }
    }

    /// Begin an emerald charge: record a pending `emerald_ops` row (idempotent on
    /// `idem_key`), auto-link the Minecraft character to the MoyMoy account, send
    /// the consume command to the mod, and return a pollable op (`GET /wallet/op`).
    /// The balance is credited later, on the mod's ack — to `account_id` (the
    /// MoyMoy account), while consumption is routed by `mc_uuid` (the character).
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
        let bus = match &self.bus {
            Some(b) => b,
            None => return Ok(json!({ "ok": false, "error": "mc_unavailable" })),
        };

        // 1. Create (or replay) the op + link the character in one transaction.
        let pool = self.pool.clone();
        let ik = idem_key.to_string();
        let aid = account_id.to_string();
        let muuid = mc_uuid.to_string();
        let mcid_owned = mcid.map(str::to_string);
        let (op_id, fresh) = tokio::task::spawn_blocking(move || -> Result<(String, bool), ApiError> {
            let mut conn = pool.get()?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(prev) = db::idem_get(&tx, &ik, "charge")? {
                let v: Value = serde_json::from_str(&prev).unwrap_or_else(|_| json!({}));
                let op = v.get("op_id").and_then(Value::as_str).unwrap_or("").to_string();
                return Ok((op, false));
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
            // runtime-attested in-world).
            crate::identity::link_mc(&tx, &aid, &muuid, mcid_owned.as_deref())?;
            let resp = json!({ "ok": true, "op_id": op_id, "state": "pending" });
            db::idem_put(&tx, &ik, "charge", &resp.to_string())?;
            tx.commit()?;
            Ok((op_id, true))
        })
        .await??;

        if op_id.is_empty() {
            return Ok(json!({ "ok": false, "error": "charge_failed" }));
        }
        if !fresh {
            return Ok(json!({ "ok": true, "op_id": op_id, "state": "pending", "duplicate": true }));
        }

        // 2. Ask the mod to consume the emeralds (auto-routed to the live server
        //    by the character's mc_uuid).
        let uuid = match Uuid::parse_str(mc_uuid) {
            Ok(u) => u,
            Err(_) => {
                self.set_state(&op_id, "failed").await;
                return Ok(json!({ "ok": false, "error": "bad_uuid", "op_id": op_id }));
            }
        };
        let state = match bus.send_charge(&uuid, &op_id, idem_key, amount).await {
            SendOutcome::Sent => {
                self.set_state(&op_id, "sent").await;
                "sent"
            }
            // Not delivered now — leave 'pending'; reconciliation retries later.
            SendOutcome::PlayerOffline | SendOutcome::BusDown | SendOutcome::Error(_) => "pending",
        };
        Ok(json!({ "ok": true, "op_id": op_id, "state": state }))
    }

    /// Re-send non-terminal ops so a dropped command/ack eventually settles
    /// (at-least-once; the mod is op-idempotent and re-acks with the same
    /// consumed amount). Called on a timer + at startup from `main`.
    pub async fn reconcile(&self) {
        let Some(bus) = &self.bus else { return };
        let pool = self.pool.clone();
        let ops: Vec<(String, String, String, i64)> = match tokio::task::spawn_blocking(move || {
            let conn = match pool.get() {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let mut stmt = match conn.prepare(
                "SELECT op_id, idem_key, mc_uuid, requested_amount FROM emerald_ops \
                 WHERE state IN ('pending','sent') ORDER BY created_unix_ms ASC LIMIT 50",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .and_then(|m| m.collect::<rusqlite::Result<Vec<_>>>())
                .unwrap_or_default()
        })
        .await
        {
            Ok(v) => v,
            Err(_) => return,
        };

        // Re-send routed by the character's mc_uuid; the credit on settle still
        // lands on the op's account_id (see settle_ack).
        for (op_id, idem_key, mc_uuid, amount) in ops {
            if let Ok(uuid) = Uuid::parse_str(&mc_uuid) {
                if let SendOutcome::Sent = bus.send_charge(&uuid, &op_id, &idem_key, amount).await {
                    self.set_state(&op_id, "sent").await;
                }
            }
        }
    }

    /// Update an op's state (best-effort; failures are logged, not fatal).
    async fn set_state(&self, op_id: &str, state: &str) {
        let pool = self.pool.clone();
        let op_id = op_id.to_string();
        let state = state.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(conn) = pool.get() {
                let _ = conn.execute(
                    "UPDATE emerald_ops SET state = ?2, updated_unix_ms = ?3 \
                     WHERE op_id = ?1 AND state NOT IN ('settled','failed')",
                    params![op_id, state, now_ms()],
                );
            }
        })
        .await;
    }
}

/// Settle a mod ack into the ledger. `ack` = `{op_id, status, settled}`.
/// Idempotent: an op already in a terminal state is ignored, so a duplicate ack
/// never double-credits. Credits the balance ONLY on a successful consume.
pub fn settle_ack(conn: &mut Connection, ack: &Value) -> rusqlite::Result<()> {
    let op_id = ack.get("op_id").and_then(Value::as_str).unwrap_or("");
    if op_id.is_empty() {
        return Ok(());
    }
    let status = ack.get("status").and_then(Value::as_str).unwrap_or("");
    let settled = ack.get("settled").and_then(Value::as_i64).unwrap_or(0);

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
    if state == "settled" || state == "failed" {
        tx.commit()?; // terminal — idempotent no-op
        return Ok(());
    }

    let now = now_ms();
    if status == "ok" || status == "duplicate" {
        let credited = settled.clamp(0, requested);
        if credited > 0 {
            wallet::credit_charge(&tx, &account_id, credited, now, CHARGE_LABEL)?;
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
/// Works without the command bus (pure ledger read).
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
