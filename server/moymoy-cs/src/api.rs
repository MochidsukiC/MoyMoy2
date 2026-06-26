//! HTTP API (axum). The app reaches us with cross-origin `fetch()` from a
//! `mochi-internal://` / app origin, so we answer the JSON-content-type preflight
//! with a permissive CORS layer (the rein/piggleshop pattern). Every DB call runs
//! in `spawn_blocking` (rusqlite is synchronous).
//!
//! Endpoints (design-derived from "MochiOS Mobile.html"):
//!   GET  /healthz
//!   GET  /wallet/status
//!   GET  /wallet/home?mc_uuid=&mcid=
//!   GET  /wallet/history?mc_uuid=&limit=&filter=
//!   GET  /wallet/friends?mc_uuid=
//!   GET  /wallet/merchants
//!   GET  /wallet/inventory?mc_uuid=        (mod-backed; degrades when can_charge=false)
//!   POST /wallet/send      {idem_key, from_uuid|from_mcid, to_uuid|to_mcid, amount, memo?}
//!   POST /wallet/pay       {idem_key, mc_uuid|mcid, merchant_id, amount, memo?}
//!   POST /wallet/charge    {idem_key, mc_uuid, mcid?, amount}
//!   GET  /wallet/op?op_id=

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, Method};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{Any, CorsLayer};

use crate::charge::ChargeCoordinator;
use crate::db::{self, Pool};
use crate::error::ApiError;
use crate::identity::{self};
use crate::wallet::{self, TxResult};

/// Shared handler state (cheap to clone — the pool and coordinator are `Arc`-ish).
#[derive(Clone)]
pub struct AppState {
    pub pool: Pool,
    pub charge: Arc<ChargeCoordinator>,
}

impl AppState {
    fn can_charge(&self) -> bool {
        self.charge.can_charge()
    }
}

pub fn router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]);

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/wallet/status", get(status))
        .route("/wallet/home", get(home))
        .route("/wallet/history", get(history))
        .route("/wallet/friends", get(friends))
        .route("/wallet/merchants", get(merchants))
        .route("/wallet/inventory", get(inventory))
        .route("/wallet/send", post(send))
        .route("/wallet/pay", post(pay))
        .route("/wallet/charge", post(charge))
        .route("/wallet/op", get(op_status))
        // Dev-only funding affordance (MC-less E2E). Gated by MOYMOY_DEV_CREDIT=1;
        // 403 otherwise. Never enable in a real deploy.
        .route("/wallet/_dev/credit", post(dev_credit))
        .with_state(state)
        .layer(cors)
}

// ── shared query helpers ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct WhoQuery {
    mc_uuid: Option<String>,
    mcid: Option<String>,
}

/// Resolve a query's identity to a canonical account_id, or a `bad_request`.
fn require_account_id(mc_uuid: Option<&str>, mcid: Option<&str>) -> Result<String, ApiError> {
    identity::resolve_account_id(mc_uuid, mcid)
        .ok_or_else(|| ApiError::bad_request("mc_uuid or mcid required"))
}

// ── GET handlers ─────────────────────────────────────────────────────────────

async fn status(State(st): State<AppState>) -> impl IntoResponse {
    Json(json!({ "ok": true, "app": "moymoy", "can_charge": st.can_charge() }))
}

async fn home(
    State(st): State<AppState>,
    Query(q): Query<WhoQuery>,
) -> Result<Json<Value>, ApiError> {
    let account_id = require_account_id(q.mc_uuid.as_deref(), q.mcid.as_deref())?;
    let can_charge = st.can_charge();
    let view = blocking(st.pool, move |conn| {
        wallet::home(conn, &account_id, q.mcid.as_deref()).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({
        "ok": true,
        "balance": view.balance,
        "profile": view.profile,
        "txns": view.txns,
        "can_charge": can_charge,
    })))
}

#[derive(Deserialize)]
struct HistoryQuery {
    mc_uuid: Option<String>,
    mcid: Option<String>,
    limit: Option<i64>,
    filter: Option<String>,
}

async fn history(
    State(st): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let account_id = require_account_id(q.mc_uuid.as_deref(), q.mcid.as_deref())?;
    let limit = q.limit.unwrap_or(50);
    let filter = q.filter.unwrap_or_else(|| "all".to_string());
    let txns = blocking(st.pool, move |conn| {
        wallet::history(conn, &account_id, limit, &filter).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({ "ok": true, "txns": txns })))
}

async fn friends(
    State(st): State<AppState>,
    Query(q): Query<WhoQuery>,
) -> Result<Json<Value>, ApiError> {
    let account_id = require_account_id(q.mc_uuid.as_deref(), q.mcid.as_deref())?;
    let list = blocking(st.pool, move |conn| {
        wallet::friends(conn, &account_id).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({ "ok": true, "friends": list })))
}

async fn merchants(State(st): State<AppState>) -> Result<Json<Value>, ApiError> {
    let list = blocking(st.pool, move |conn| {
        wallet::merchants(conn).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({ "ok": true, "merchants": list })))
}

async fn inventory(
    State(st): State<AppState>,
    Query(q): Query<WhoQuery>,
) -> Result<Json<Value>, ApiError> {
    let account_id = require_account_id(q.mc_uuid.as_deref(), q.mcid.as_deref())?;
    if !st.can_charge() {
        return Ok(Json(json!({
            "ok": false, "error": "mc_unavailable", "can_charge": false,
            "emeralds": 0, "blocks": 0, "chargeable": 0,
        })));
    }
    let inv = st.charge.query_inventory(&account_id).await?;
    Ok(Json(json!({
        "ok": true, "can_charge": true,
        "emeralds": inv.emeralds, "blocks": inv.blocks, "chargeable": inv.chargeable,
    })))
}

#[derive(Deserialize)]
struct OpQuery {
    op_id: String,
}

async fn op_status(
    State(st): State<AppState>,
    Query(q): Query<OpQuery>,
) -> Result<Json<Value>, ApiError> {
    let op = blocking(st.pool, move |conn| {
        crate::charge::op_view(conn, &q.op_id).map_err(ApiError::from)
    })
    .await?;
    match op {
        Some(v) => Ok(Json(json!({ "ok": true, "op": v }))),
        None => Ok(Json(json!({ "ok": false, "error": "unknown_op" }))),
    }
}

// ── POST handlers ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SendReq {
    idem_key: String,
    from_uuid: Option<String>,
    from_mcid: Option<String>,
    to_uuid: Option<String>,
    to_mcid: Option<String>,
    amount: i64,
}

async fn send(
    State(st): State<AppState>,
    Json(req): Json<SendReq>,
) -> Result<Json<Value>, ApiError> {
    if req.idem_key.trim().is_empty() {
        return Err(ApiError::bad_request("idem_key required"));
    }
    let from_id = require_account_id(req.from_uuid.as_deref(), req.from_mcid.as_deref())?;
    let to_id = match identity::resolve_account_id(req.to_uuid.as_deref(), req.to_mcid.as_deref()) {
        Some(id) => id,
        None => return Ok(Json(json!({ "ok": false, "error": "unknown_target" }))),
    };
    let to_name = req.to_mcid.clone();
    let from_name = req.from_mcid.clone();
    let label = to_name
        .clone()
        .map(|n| format!("{n} へ送金"))
        .unwrap_or_else(|| "送金".to_string());

    let value = blocking(st.pool, move |conn| {
        // Single BEGIN IMMEDIATE: the idem check-reserve-execute-record is one
        // atomic unit, so concurrent retries of the same idem_key serialize and
        // the second one replays (no TOCTOU double-spend).
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(prev) = db::idem_get(&tx, &req.idem_key, "send")? {
            return Ok(replay(prev)); // tx drops → rollback (read-only path)
        }
        let result = wallet::transfer(
            &tx,
            &from_id,
            from_name.as_deref(),
            &to_id,
            to_name.as_deref(),
            req.amount,
            "send",
            &label,
        )?;
        let (v, ok) = tx_result_json(result);
        if ok {
            db::idem_put(&tx, &req.idem_key, "send", &v.to_string())?;
        }
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct PayReq {
    idem_key: String,
    mc_uuid: Option<String>,
    mcid: Option<String>,
    merchant_id: String,
    amount: i64,
}

async fn pay(State(st): State<AppState>, Json(req): Json<PayReq>) -> Result<Json<Value>, ApiError> {
    if req.idem_key.trim().is_empty() {
        return Err(ApiError::bad_request("idem_key required"));
    }
    let from_id = require_account_id(req.mc_uuid.as_deref(), req.mcid.as_deref())?;
    let from_name = req.mcid.clone();

    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(prev) = db::idem_get(&tx, &req.idem_key, "pay")? {
            return Ok(replay(prev));
        }
        let (to_id, merchant_name) = match wallet::merchant_account(&tx, &req.merchant_id)? {
            Some(m) => m,
            None => return Ok(json!({ "ok": false, "error": "unknown_target" })),
        };
        let result = wallet::transfer(
            &tx,
            &from_id,
            from_name.as_deref(),
            &to_id,
            Some(&merchant_name),
            req.amount,
            "pay",
            &merchant_name,
        )?;
        let (v, ok) = tx_result_json(result);
        if ok {
            db::idem_put(&tx, &req.idem_key, "pay", &v.to_string())?;
        }
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct ChargeReq {
    idem_key: String,
    mc_uuid: Option<String>,
    mcid: Option<String>,
    amount: i64,
}

async fn charge(
    State(st): State<AppState>,
    Json(req): Json<ChargeReq>,
) -> Result<Json<Value>, ApiError> {
    if req.idem_key.trim().is_empty() {
        return Err(ApiError::bad_request("idem_key required"));
    }
    let account_id = require_account_id(req.mc_uuid.as_deref(), req.mcid.as_deref())?;
    if !st.can_charge() {
        return Ok(Json(json!({ "ok": false, "error": "mc_unavailable" })));
    }
    let value = st
        .charge
        .begin_charge(&req.idem_key, &account_id, req.mcid.as_deref(), req.amount)
        .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct DevCreditReq {
    mc_uuid: Option<String>,
    mcid: Option<String>,
    amount: i64,
}

/// Dev-only: credit an account directly (MC-less E2E funding). Gated by
/// `MOYMOY_DEV_CREDIT=1`; returns 403 otherwise.
async fn dev_credit(
    State(st): State<AppState>,
    Json(req): Json<DevCreditReq>,
) -> Result<Json<Value>, ApiError> {
    if !crate::env_flag("MOYMOY_DEV_CREDIT", false) {
        return Err(ApiError::forbidden("dev credit disabled (set MOYMOY_DEV_CREDIT=1)"));
    }
    if req.amount <= 0 || req.amount > wallet::MAX_AMOUNT {
        return Err(ApiError::bad_request("bad amount"));
    }
    let account_id = require_account_id(req.mc_uuid.as_deref(), req.mcid.as_deref())?;
    let mcid = req.mcid.clone();
    let balance = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        identity::get_or_create(&tx, &account_id, mcid.as_deref())?;
        let after = wallet::credit_charge(&tx, &account_id, req.amount, db::now_ms(), "開発用クレジット")?;
        tx.commit()?;
        Ok::<i64, ApiError>(after)
    })
    .await?;
    Ok(Json(json!({ "ok": true, "balance": balance })))
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Run a blocking DB closure on the blocking pool, mapping pool/join failures to
/// `ApiError`.
async fn blocking<T, F>(pool: Pool, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&mut rusqlite::Connection) -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut conn = pool.get()?;
        f(&mut conn)
    })
    .await?
}

/// Map a [`TxResult`] to `(json, success)`.
fn tx_result_json(r: TxResult) -> (Value, bool) {
    match r {
        TxResult::Ok {
            tx_id,
            balance_after,
            counterparty_name,
        } => (
            json!({ "ok": true, "tx_id": tx_id, "balance": balance_after, "counterparty": counterparty_name }),
            true,
        ),
        TxResult::BadAmount => (json!({ "ok": false, "error": "bad_amount" }), false),
        TxResult::SelfTransfer => (json!({ "ok": false, "error": "self_transfer" }), false),
        TxResult::UnknownTarget => (json!({ "ok": false, "error": "unknown_target" }), false),
        TxResult::Insufficient { balance } => (
            json!({ "ok": false, "error": "insufficient", "balance": balance }),
            false,
        ),
    }
}

/// Parse a frozen idempotency response back to JSON, tagging it as a replay.
fn replay(stored: String) -> Value {
    match serde_json::from_str::<Value>(&stored) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("duplicate".to_string(), json!(true));
            }
            v
        }
        Err(_) => json!({ "ok": true, "duplicate": true }),
    }
}
