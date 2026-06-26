//! Emerald-charge coordinator: the bridge between the in-world mod (truth of
//! emerald consumption) and the wallet (truth of balance), reconciled through the
//! `emerald_ops` ledger.
//!
//! Consistency model (DEV.md): consume-first with at-least-once delivery + an
//! op-keyed idempotent settlement. The balance is credited ONLY when the mod's
//! settlement ack arrives (state → settled), never on send — so a lost ack never
//! mints eme that no emerald paid for, and a duplicate ack never double-credits.
//!
//! Milestone 1 (this file) implements the ledger read side (`op_view`) and the
//! degraded contract. The command-bus send/receive + reconciliation is wired in
//! task#5 (`begin_charge` / `query_inventory` are only reached when
//! `can_charge()` is true, i.e. once the bus is connected).

use serde_json::{json, Value};

use crate::command_bus::CommandBus;
use crate::db::Pool;
use crate::error::ApiError;

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
    #[allow(dead_code)]
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

    /// Query the player's chargeable inventory via the mod. Only reached when
    /// `can_charge()` is true (the inventory handler short-circuits otherwise).
    pub async fn query_inventory(&self, _account_id: &str) -> Result<Inventory, ApiError> {
        // TODO(task#5): round-trip `inventory.query` over the command bus.
        Err(ApiError::internal(
            "emerald charge not wired yet (task#5)",
        ))
    }

    /// Begin an emerald charge: record a pending `emerald_ops` row, send the
    /// consume command to the mod, and return a pending op the app can poll via
    /// `GET /wallet/op`. Only reached when `can_charge()` is true.
    pub async fn begin_charge(
        &self,
        _idem_key: &str,
        _account_id: &str,
        _mcid: Option<&str>,
        _amount: i64,
    ) -> Result<Value, ApiError> {
        // TODO(task#5): idempotency check → insert emerald_ops(pending) →
        // resolve_mcid → reliable_send(emerald.charge) → return {ok, op_id, state}.
        Err(ApiError::internal(
            "emerald charge not wired yet (task#5)",
        ))
    }
}

/// Read an `emerald_ops` row as a pollable view, if it exists. Works without the
/// command bus (pure ledger read).
pub fn op_view(conn: &rusqlite::Connection, op_id: &str) -> rusqlite::Result<Option<Value>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT op_id, direction, requested_amount, settled_amount, state, updated_unix_ms \
         FROM emerald_ops WHERE op_id = ?1",
        [op_id],
        |row| {
            Ok(json!({
                "op_id": row.get::<_, String>(0)?,
                "direction": row.get::<_, String>(1)?,
                "requested_amount": row.get::<_, i64>(2)?,
                "settled_amount": row.get::<_, Option<i64>>(3)?,
                "state": row.get::<_, String>(4)?,
                "updated_ms": row.get::<_, i64>(5)?,
            }))
        },
    )
    .optional()
}
