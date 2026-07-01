//! HTTP API (axum). The app reaches us with cross-origin `fetch()` from a
//! `mochi-internal://` / app origin, so we answer the JSON-content-type preflight
//! with a permissive CORS layer (the rein/piggleshop pattern). Every DB call runs
//! in `spawn_blocking` (rusqlite is synchronous).
//!
//! Identity (v2): callers authenticate with a MoyMoy account (handle + PIN — see
//! [`crate::auth`]). Wallet endpoints resolve the account from the
//! `X-MoyMoy-Session` header via the [`AuthedAccount`] extractor — no more
//! self-asserted mc_uuid. The Minecraft UUID is supplied only for charge /
//! inventory (the character whose emeralds to consume).
//!
//! Endpoints:
//!   GET  /healthz
//!   GET  /wallet/status
//!   POST /auth/register   {handle, display_name, pin, phone_id?}
//!   POST /auth/login      {handle, pin, phone_id?}
//!   POST /auth/logout     (X-MoyMoy-Session)
//!   GET  /auth/me         (auth)
//!   GET  /auth/lookup?handle=            (auth — send-target resolution)
//!   GET  /wallet/home     (auth)
//!   GET  /wallet/history?limit=&filter=  (auth)
//!   GET  /wallet/friends  (auth)
//!   GET  /wallet/merchants (auth)
//!   GET  /wallet/inventory?mc_uuid=&mcid= (auth; mod-backed)
//!   POST /wallet/send     {idem_key, to_handle, amount}            (auth)
//!   POST /wallet/pay      {idem_key, merchant_id, amount}          (auth)
//!   POST /wallet/charge   {idem_key, amount, mc_uuid, mcid?}       (auth)
//!   GET  /wallet/op?op_id=                                         (auth)

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderName, Method};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{Any, CorsLayer};

use crate::auth::{self, AuthedAccount, LoginOutcome, RegisterOutcome};
use crate::charge::ChargeCoordinator;
use crate::db::{self, Pool};
use crate::error::ApiError;
use crate::identity;
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
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            HeaderName::from_static(auth::SESSION_HEADER),
        ]);

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/wallet/status", get(status))
        // Auth (independent MoyMoy accounts).
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/auth/lookup", get(lookup))
        // Wallet (session-authenticated).
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

// ── status ───────────────────────────────────────────────────────────────────

async fn status(State(st): State<AppState>) -> impl IntoResponse {
    Json(json!({ "ok": true, "app": "moymoy", "can_charge": st.can_charge() }))
}

// ── auth ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RegisterReq {
    handle: String,
    display_name: String,
    pin: String,
    phone_id: Option<String>,
}

async fn register(
    State(st): State<AppState>,
    Json(req): Json<RegisterReq>,
) -> Result<Json<Value>, ApiError> {
    let value = blocking(st.pool, move |conn| {
        // IMMEDIATE so the handle-uniqueness check + insert are one atomic unit.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let outcome = auth::register(
            &tx,
            &req.handle,
            &req.display_name,
            &req.pin,
            req.phone_id.as_deref(),
        )?;
        let v = match outcome {
            RegisterOutcome::Ok(m) => json!({ "ok": true, "session": m.token, "account": m.account }),
            RegisterOutcome::BadHandle => json!({ "ok": false, "error": "bad_handle" }),
            RegisterOutcome::BadPin => json!({ "ok": false, "error": "bad_pin" }),
            RegisterOutcome::BadDisplayName => json!({ "ok": false, "error": "bad_display_name" }),
            RegisterOutcome::HandleTaken => json!({ "ok": false, "error": "handle_taken" }),
        };
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct LoginReq {
    handle: String,
    pin: String,
    phone_id: Option<String>,
}

async fn login(
    State(st): State<AppState>,
    Json(req): Json<LoginReq>,
) -> Result<Json<Value>, ApiError> {
    let value = blocking(st.pool, move |conn| {
        // IMMEDIATE so the attempt-counter update / session mint commit atomically.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let outcome = auth::login(&tx, &req.handle, &req.pin, req.phone_id.as_deref())?;
        let v = match outcome {
            LoginOutcome::Ok(m) => json!({ "ok": true, "session": m.token, "account": m.account }),
            LoginOutcome::Invalid => json!({ "ok": false, "error": "invalid_credentials" }),
            LoginOutcome::Locked { retry_after_ms } => {
                json!({ "ok": false, "error": "locked", "retry_after_ms": retry_after_ms })
            }
        };
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let token = headers
        .get(auth::SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(tok) = token {
        blocking(st.pool, move |conn| {
            auth::logout(conn, &tok)?;
            Ok::<(), ApiError>(())
        })
        .await?;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn me(State(st): State<AppState>, acct: AuthedAccount) -> Result<Json<Value>, ApiError> {
    let id = acct.account_id;
    let (view, links) = blocking(st.pool, move |conn| {
        let view = auth::account_view(conn, &id)?
            .ok_or_else(|| ApiError::unauthorized("account no longer exists"))?;
        let links = identity::linked_mc(conn, &id)?;
        Ok::<_, ApiError>((view, links))
    })
    .await?;
    Ok(Json(json!({ "ok": true, "account": view, "linked_mc": links })))
}

#[derive(Deserialize)]
struct LookupQuery {
    handle: String,
}

async fn lookup(
    State(st): State<AppState>,
    _acct: AuthedAccount,
    Query(q): Query<LookupQuery>,
) -> Result<Json<Value>, ApiError> {
    let v = blocking(st.pool, move |conn| {
        let found = auth::lookup_handle(conn, &q.handle)?;
        Ok::<Value, ApiError>(match found {
            Some(a) => json!({ "ok": true, "account": a }),
            None => json!({ "ok": false, "error": "not_found" }),
        })
    })
    .await?;
    Ok(Json(v))
}

// ── wallet GET ───────────────────────────────────────────────────────────────

async fn home(State(st): State<AppState>, acct: AuthedAccount) -> Result<Json<Value>, ApiError> {
    let can_charge = st.can_charge();
    let id = acct.account_id;
    let view = blocking(st.pool, move |conn| {
        wallet::home(conn, &id)?.ok_or_else(|| ApiError::internal("authed account missing"))
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
    limit: Option<i64>,
    filter: Option<String>,
}

async fn history(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let id = acct.account_id;
    let limit = q.limit.unwrap_or(50);
    let filter = q.filter.unwrap_or_else(|| "all".to_string());
    let txns = blocking(st.pool, move |conn| {
        wallet::history(conn, &id, limit, &filter).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({ "ok": true, "txns": txns })))
}

async fn friends(State(st): State<AppState>, acct: AuthedAccount) -> Result<Json<Value>, ApiError> {
    let id = acct.account_id;
    let list = blocking(st.pool, move |conn| {
        wallet::friends(conn, &id).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({ "ok": true, "friends": list })))
}

async fn merchants(
    State(st): State<AppState>,
    _acct: AuthedAccount,
) -> Result<Json<Value>, ApiError> {
    let list = blocking(st.pool, move |conn| {
        wallet::merchants(conn).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({ "ok": true, "merchants": list })))
}

#[derive(Deserialize)]
struct InventoryQuery {
    mc_uuid: Option<String>,
    #[allow(dead_code)]
    mcid: Option<String>,
}

async fn inventory(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Query(q): Query<InventoryQuery>,
) -> Result<Json<Value>, ApiError> {
    if !st.can_charge() {
        return Ok(Json(json!({
            "ok": false, "error": "mc_unavailable", "can_charge": false,
            "emeralds": 0, "blocks": 0, "chargeable": 0,
        })));
    }
    let mc_uuid = match q.mc_uuid.as_deref().and_then(identity::normalize_uuid) {
        Some(u) => u,
        None => {
            return Ok(Json(json!({
                "ok": false, "error": "no_character", "can_charge": true,
                "emeralds": 0, "blocks": 0, "chargeable": 0,
            })))
        }
    };
    // R007: don't reveal a character's inventory to an account that doesn't own
    // it. Unclaimed (first charge) or self-owned is fine; another account's is not.
    let id = acct.account_id.clone();
    let mu = mc_uuid.clone();
    let claimed_by_other = blocking(st.pool.clone(), move |conn| {
        Ok::<bool, ApiError>(matches!(identity::mc_link_owner(conn, &mu)?, Some(o) if o != id))
    })
    .await?;
    if claimed_by_other {
        return Ok(Json(json!({
            "ok": false, "error": "character_claimed", "can_charge": true,
            "emeralds": 0, "blocks": 0, "chargeable": 0,
        })));
    }
    let inv = st.charge.query_inventory(&mc_uuid).await?;
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
    acct: AuthedAccount,
    Query(q): Query<OpQuery>,
) -> Result<Json<Value>, ApiError> {
    let id = acct.account_id;
    let op = blocking(st.pool, move |conn| {
        crate::charge::op_view(conn, &q.op_id).map_err(ApiError::from)
    })
    .await?;
    // Only the owning account may poll an op (don't leak others' op state).
    match op {
        Some((owner, view)) if owner == id => Ok(Json(json!({ "ok": true, "op": view }))),
        _ => Ok(Json(json!({ "ok": false, "error": "unknown_op" }))),
    }
}

// ── wallet POST ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SendReq {
    idem_key: String,
    to_handle: String,
    amount: i64,
}

async fn send(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<SendReq>,
) -> Result<Json<Value>, ApiError> {
    if req.idem_key.trim().is_empty() {
        return Err(ApiError::bad_request("idem_key required"));
    }
    let from_id = acct.account_id;
    let value = blocking(st.pool, move |conn| {
        // Single BEGIN IMMEDIATE: idem check-reserve-execute-record is one atomic
        // unit, so concurrent retries of the same idem_key serialize and the
        // second one replays (no TOCTOU double-spend).
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(prev) = db::idem_get(&tx, &req.idem_key, "send")? {
            return Ok(replay(prev)); // tx drops → rollback (read-only path)
        }
        let to = match auth::lookup_handle(&tx, &req.to_handle)? {
            Some(a) => a,
            None => return Ok::<Value, ApiError>(json!({ "ok": false, "error": "unknown_target" })),
        };
        let label = format!("@{} へ送金", to.handle);
        let result = wallet::transfer(&tx, &from_id, &to.account_id, req.amount, "send", &label)?;
        let (v, ok) = tx_result_json(result);
        if ok {
            db::idem_put(&tx, &req.idem_key, "send", &v.to_string())?;
        }
        tx.commit()?;
        Ok(v)
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct PayReq {
    idem_key: String,
    merchant_id: String,
    amount: i64,
}

async fn pay(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<PayReq>,
) -> Result<Json<Value>, ApiError> {
    if req.idem_key.trim().is_empty() {
        return Err(ApiError::bad_request("idem_key required"));
    }
    let from_id = acct.account_id;
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(prev) = db::idem_get(&tx, &req.idem_key, "pay")? {
            return Ok(replay(prev));
        }
        let (to_id, merchant_name) = match wallet::merchant_account(&tx, &req.merchant_id)? {
            Some(m) => m,
            None => return Ok::<Value, ApiError>(json!({ "ok": false, "error": "unknown_target" })),
        };
        let result = wallet::transfer(&tx, &from_id, &to_id, req.amount, "pay", &merchant_name)?;
        let (v, ok) = tx_result_json(result);
        if ok {
            db::idem_put(&tx, &req.idem_key, "pay", &v.to_string())?;
        }
        tx.commit()?;
        Ok(v)
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct ChargeReq {
    idem_key: String,
    amount: i64,
    mc_uuid: Option<String>,
    mcid: Option<String>,
}

async fn charge(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<ChargeReq>,
) -> Result<Json<Value>, ApiError> {
    if req.idem_key.trim().is_empty() {
        return Err(ApiError::bad_request("idem_key required"));
    }
    if !st.can_charge() {
        return Ok(Json(json!({ "ok": false, "error": "mc_unavailable" })));
    }
    let mc_uuid = match req.mc_uuid.as_deref().and_then(identity::normalize_uuid) {
        Some(u) => u,
        None => return Ok(Json(json!({ "ok": false, "error": "no_character" }))),
    };
    let value = st
        .charge
        .begin_charge(
            &req.idem_key,
            &acct.account_id,
            &mc_uuid,
            req.mcid.as_deref(),
            req.amount,
        )
        .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct DevCreditReq {
    handle: String,
    amount: i64,
}

/// Dev-only: credit an account directly by handle (MC-less E2E funding). Gated by
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
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let acct = match auth::lookup_handle(&tx, &req.handle)? {
            Some(a) => a,
            None => return Ok::<Value, ApiError>(json!({ "ok": false, "error": "unknown_target" })),
        };
        let after = wallet::credit_charge(
            &tx,
            &acct.account_id,
            req.amount,
            db::now_ms(),
            "開発用クレジット",
        )?;
        tx.commit()?;
        Ok(json!({ "ok": true, "balance": after }))
    })
    .await?;
    Ok(Json(value))
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
