//! HTTP API (axum). The app reaches us with cross-origin `fetch()` from a
//! `mochi-internal://` / app origin, so we answer the JSON-content-type preflight
//! with a permissive CORS layer (the rein/piggleshop pattern). Every DB call runs
//! in `spawn_blocking` (rusqlite is synchronous).
//!
//! Identity (v2): callers authenticate with a MoyMoy account (handle + PIN — see
//! [`crate::auth`]). Wallet endpoints resolve the account from the
//! `X-MoyMoy-Session` header via the [`AuthedAccount`] extractor.
//!
//! **The session is the only thing that says whose wallet this is.** Since v5 a
//! Hub-signed assertion ([`crate::attest`]) decides the separate question of
//! which in-world character's emeralds a request may consume — the app no longer
//! asserts an `mc_uuid` of its own, and an assertion is never accepted in place
//! of a session. See the `attest` module docs for the full boundary.
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
//!   GET  /wallet/history?limit=&filter=  (auth — all|pay|send|charge|withdraw)
//!   GET  /wallet/friends  (auth)
//!   GET  /wallet/merchants (auth)
//!   POST /wallet/attest/challenge {purpose}   (auth)
//!   POST /wallet/attest/session   {assertion} (auth — confirm the character)
//!   GET  /wallet/inventory (auth; mod-backed, no query args)
//!   POST /wallet/send     {idem_key, to_handle, amount, pin?, otp?}  (auth)
//!   POST /wallet/charge   {idem_key, amount, assertion?}           (auth)
//!   POST /wallet/withdraw {idem_key, amount, assertion?, pin?, otp?} (auth)
//!   GET  /wallet/op?op_id=                                         (auth)
//!   POST /wallet/stepup/otp                                        (auth)
//!   GET  /wallet/payment/intent?intent_id=                         (auth)
//!   POST /wallet/payment/approve {intent_id, pin, otp?}            (auth)
//!   POST /wallet/payment/decline {intent_id}                       (auth)
//!   POST /merchant/portal/register {name, sub?, pin, glyph?, pal?} (auth + PIN)
//!   POST /merchant/portal/key      {merchant_id, pin}              (auth + PIN)
//!   POST /merchant/portal/status   {merchant_id, status, pin}      (auth + PIN)
//!   POST /merchant/portal/limits   {merchant_id, pin, …}           (auth + PIN)
//!   GET  /merchant/portal/list                                     (auth)
//!   POST /merchant/v1/intent/create {…}                            (API key)
//!   GET  /merchant/v1/intent?intent_id=                            (API key)
//!   POST /merchant/v1/intent/cancel {intent_id}                    (API key)
//!
//! `/wallet/pay` is gone. It sent a client-chosen amount to a client-chosen
//! merchant, which is the thing `payment_intents` exists to make impossible: the
//! amount and the recipient now come from a record the merchant created and the
//! client can only point at.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderName, Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tower_http::cors::{Any, CorsLayer};

use crate::attest::{self, AttestPurpose, AttestVerifier, ChallengeStore, CharSessionStore};
use crate::auth::{
    self, AuthedAccount, CredsOutcome, LockoutPolicy, PinAttempt, RegisterOutcome, VerifiedSignup,
};
use crate::charge::ChargeCoordinator;
use crate::db::{self, now_ms, Pool};
use crate::error::ApiError;
use crate::identity;
use crate::merchant::{self, IssueGuard, MerchantAuth, RateLimiter};
use crate::otp::{self, CreateOtp, Mailer, PendingSignup, VerifyOtp};
use crate::payments::{self, CreateOutcome, NewIntent};
use crate::riskauth::{self, PinBackoff, Requirement};
use crate::wallet::{self, TxResult};

/// Shared handler state (cheap to clone — the pool and coordinator are `Arc`-ish).
#[derive(Clone)]
pub struct AppState {
    pub pool: Pool,
    pub charge: Arc<ChargeCoordinator>,
    pub mailer: Mailer,
    /// Verifies Hub-signed host attestations (holds the fetched public key).
    pub attest: Arc<AttestVerifier>,
    /// Anti-replay nonces for those assertions.
    pub challenges: Arc<ChallengeStore>,
    /// The character each account most recently confirmed (inventory reads).
    pub char_sessions: Arc<CharSessionStore>,
    /// Per-merchant call counters (creation / lookup / registration).
    pub rate: Arc<RateLimiter>,
    /// Per-session PIN backoff for the money paths.
    pub pin_backoff: Arc<PinBackoff>,
}

impl AppState {
    fn can_charge(&self) -> bool {
        self.charge.can_charge()
    }
    fn email_enabled(&self) -> bool {
        self.mailer.enabled()
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
        // Auth (independent MoyMoy accounts + email verification / 2FA / recovery).
        .route("/auth/config", get(auth_config))
        .route("/auth/register", post(register))
        .route("/auth/register/verify", post(register_verify))
        .route("/auth/login", post(login))
        .route("/auth/login/verify", post(login_verify))
        .route("/auth/recover/start", post(recover_start))
        .route("/auth/recover/verify", post(recover_verify))
        .route("/auth/logout", post(logout))
        .route("/auth/me", get(me))
        .route("/auth/lookup", get(lookup))
        // Wallet (session-authenticated).
        .route("/wallet/home", get(home))
        .route("/wallet/history", get(history))
        .route("/wallet/friends", get(friends))
        .route("/wallet/merchants", get(merchants))
        // Host attestation (DEV.md §7.3.10 G4): challenge → assertion → the
        // character whose emeralds may be consumed.
        .route("/wallet/attest/challenge", post(attest_challenge))
        .route("/wallet/attest/session", post(attest_session))
        .route("/wallet/inventory", get(inventory))
        .route("/wallet/send", post(send))
        .route("/wallet/charge", post(charge))
        .route("/wallet/withdraw", post(withdraw))
        .route("/wallet/op", get(op_status))
        // Step-up second factor for the movements riskauth escalates.
        .route("/wallet/stepup/otp", post(stepup_otp))
        // EC payment, payer side. The approval screen's only source of truth.
        .route("/wallet/payment/intent", get(payment_intent))
        .route("/wallet/payment/approve", post(payment_approve))
        .route("/wallet/payment/decline", post(payment_decline))
        // Merchant portal (session + PIN): the credential-issuing half.
        .route("/merchant/portal/register", post(portal_register))
        .route("/merchant/portal/key", post(portal_rotate_key))
        .route("/merchant/portal/status", post(portal_set_status))
        .route("/merchant/portal/limits", post(portal_set_limits))
        .route("/merchant/portal/list", get(portal_list))
        // Merchant API (Bearer moy_sk_…): intents only, never a balance.
        .route("/merchant/v1/intent/create", post(intent_create))
        .route("/merchant/v1/intent", get(intent_get))
        .route("/merchant/v1/intent/cancel", post(intent_cancel))
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

/// Whether email-backed features (verify / 2FA / recovery) are active. The app
/// shows the email UI only when this is true.
async fn auth_config(State(st): State<AppState>) -> impl IntoResponse {
    Json(json!({ "ok": true, "email_enabled": st.email_enabled() }))
}

#[derive(Deserialize)]
struct RegisterReq {
    handle: String,
    display_name: String,
    pin: String,
    email: Option<String>,
    phone_id: Option<String>,
}

enum SignupStart {
    Issued(String),
    TooSoon(i64),
    HandleTaken,
    EmailTaken,
}

/// Start a signup. With email enabled: validate, email a code, return
/// `pending:"verify_email"` (the account is created on verify). With email
/// disabled (no SMTP): degrade to immediate handle+PIN creation.
async fn register(
    State(st): State<AppState>,
    Json(req): Json<RegisterReq>,
) -> Result<Json<Value>, ApiError> {
    if !st.email_enabled() {
        let value = blocking(st.pool, move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let outcome = auth::register(
                &tx,
                &req.handle,
                &req.display_name,
                &req.pin,
                req.phone_id.as_deref(),
            )?;
            let v = match outcome {
                RegisterOutcome::Ok(m) => {
                    json!({ "ok": true, "session": m.token, "account": m.account })
                }
                RegisterOutcome::BadHandle => json!({ "ok": false, "error": "bad_handle" }),
                RegisterOutcome::BadPin => json!({ "ok": false, "error": "bad_pin" }),
                RegisterOutcome::BadDisplayName => {
                    json!({ "ok": false, "error": "bad_display_name" })
                }
                RegisterOutcome::HandleTaken => json!({ "ok": false, "error": "handle_taken" }),
            };
            tx.commit()?;
            Ok::<Value, ApiError>(v)
        })
        .await?;
        return Ok(Json(value));
    }

    // Email path: validate everything up front, then issue + email an OTP.
    let handle = match auth::valid_handle(&req.handle) {
        Some(h) => h,
        None => return Ok(Json(json!({ "ok": false, "error": "bad_handle" }))),
    };
    let display = match auth::valid_display_name(&req.display_name) {
        Some(d) => d,
        None => return Ok(Json(json!({ "ok": false, "error": "bad_display_name" }))),
    };
    if !auth::valid_pin(&req.pin) {
        return Ok(Json(json!({ "ok": false, "error": "bad_pin" })));
    }
    let email = match req.email.as_deref().and_then(otp::valid_email) {
        Some(e) => e,
        None => return Ok(Json(json!({ "ok": false, "error": "bad_email" }))),
    };
    let email_lower = email.to_lowercase();
    let handle_lower = handle.to_lowercase();
    let pin_hash = auth::hash_pin(&req.pin)?;
    let pending = PendingSignup {
        handle: handle.clone(),
        handle_lower,
        display_name: display,
        pin_hash,
    };
    let payload = serde_json::to_string(&pending).map_err(|e| ApiError::internal(e.to_string()))?;

    let el = email_lower.clone();
    let hl = pending.handle_lower.clone();
    let start = blocking(st.pool.clone(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = if auth::handle_taken(&tx, &hl)? {
            SignupStart::HandleTaken
        } else if auth::email_taken(&tx, &el)? {
            SignupStart::EmailTaken
        } else {
            match otp::create(&tx, otp::PURPOSE_SIGNUP, &el, None, Some(&payload))? {
                CreateOtp::Issued(code) => SignupStart::Issued(code),
                CreateOtp::TooSoon { retry_after_ms } => SignupStart::TooSoon(retry_after_ms),
            }
        };
        tx.commit()?;
        Ok::<SignupStart, ApiError>(out)
    })
    .await?;

    match start {
        SignupStart::HandleTaken => Ok(Json(json!({ "ok": false, "error": "handle_taken" }))),
        SignupStart::EmailTaken => Ok(Json(json!({ "ok": false, "error": "email_taken" }))),
        SignupStart::TooSoon(ms) => Ok(Json(
            json!({ "ok": false, "error": "too_soon", "retry_after_ms": ms }),
        )),
        SignupStart::Issued(code) => {
            if let Err(e) = st.mailer.send(&email, &code, otp::PURPOSE_SIGNUP).await {
                // Roll back the OTP so the resend cooldown doesn't strand the user.
                let el = email_lower.clone();
                if let Err(re) = blocking(st.pool.clone(), move |conn| {
                    otp::revoke(conn, otp::PURPOSE_SIGNUP, &el)
                })
                .await
                {
                    tracing::warn!(error = %re, "failed to roll back undelivered signup OTP");
                }
                return Err(e);
            }
            Ok(Json(
                json!({ "ok": true, "pending": "verify_email", "email": email }),
            ))
        }
    }
}

#[derive(Deserialize)]
struct RegisterVerifyReq {
    email: String,
    code: String,
    phone_id: Option<String>,
}

/// Finish a signup: verify the emailed code, then create the account + session.
async fn register_verify(
    State(st): State<AppState>,
    Json(req): Json<RegisterVerifyReq>,
) -> Result<Json<Value>, ApiError> {
    let email = match otp::valid_email(&req.email) {
        Some(e) => e,
        None => return Ok(Json(json!({ "ok": false, "error": "bad_email" }))),
    };
    let email_lower = email.to_lowercase();
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let v = match otp::verify(&tx, otp::PURPOSE_SIGNUP, &email_lower, &req.code)? {
            VerifyOtp::Ok { payload, .. } => {
                // A corrupt/missing payload is a server fault, not a wrong code.
                // Returning 500 here rolls back the transaction so the OTP is not
                // consumed, allowing the user to retry with a fresh resend.
                let raw =
                    payload.ok_or_else(|| ApiError::internal("signup OTP missing payload"))?;
                let pending: PendingSignup = serde_json::from_str(&raw)
                    .map_err(|e| ApiError::internal(format!("corrupt signup payload: {e}")))?;
                match auth::register_verified(
                    &tx,
                    &pending,
                    &email,
                    &email_lower,
                    req.phone_id.as_deref(),
                )? {
                    VerifiedSignup::Ok(m) => {
                        json!({ "ok": true, "session": m.token, "account": m.account })
                    }
                    VerifiedSignup::HandleTaken => json!({ "ok": false, "error": "handle_taken" }),
                    VerifiedSignup::EmailTaken => json!({ "ok": false, "error": "email_taken" }),
                }
            }
            VerifyOtp::Invalid => json!({ "ok": false, "error": "invalid_code" }),
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

enum LoginStart {
    Terminal(Value),
    /// `email_lower` mirrors the key passed to `otp::create` so the revoke on
    /// SMTP failure uses the identical key (symmetry with register and recover).
    TwoFactor {
        email: String,
        email_lower: String,
        code: Option<String>,
    },
}

/// Login step 1: verify handle + PIN. If the account has a verified email and
/// email is enabled, email a 2FA code and return `pending:"2fa"`; otherwise mint
/// the session directly.
async fn login(
    State(st): State<AppState>,
    Json(req): Json<LoginReq>,
) -> Result<Json<Value>, ApiError> {
    let email_enabled = st.email_enabled();
    let start = blocking(st.pool.clone(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = match auth::verify_credentials(&tx, &req.handle, &req.pin)? {
            CredsOutcome::Invalid => LoginStart::Terminal(json!({ "ok": false, "error": "invalid_credentials" })),
            CredsOutcome::Locked { retry_after_ms } => {
                LoginStart::Terminal(json!({ "ok": false, "error": "locked", "retry_after_ms": retry_after_ms }))
            }
            CredsOutcome::Ok(info) => {
                if email_enabled && info.email_verified && info.email_lower.is_some() {
                    let el = info.email_lower.clone().unwrap_or_default();
                    let em = info.email.clone().unwrap_or_default();
                    match otp::create(&tx, otp::PURPOSE_LOGIN2FA, &el, Some(&info.account_id), None)? {
                        CreateOtp::Issued(code) => LoginStart::TwoFactor { email: em, email_lower: el, code: Some(code) },
                        CreateOtp::TooSoon { .. } => LoginStart::TwoFactor { email: em, email_lower: el, code: None },
                    }
                } else {
                    let token = auth::create_session(&tx, &info.account_id, req.phone_id.as_deref())?;
                    LoginStart::Terminal(json!({ "ok": true, "session": token,
                        "account": { "account_id": info.account_id, "handle": info.handle, "display_name": info.display_name } }))
                }
            }
        };
        tx.commit()?;
        Ok::<LoginStart, ApiError>(out)
    })
    .await?;

    match start {
        LoginStart::Terminal(v) => Ok(Json(v)),
        LoginStart::TwoFactor {
            email,
            email_lower,
            code,
        } => {
            if let Some(c) = code {
                if let Err(e) = st.mailer.send(&email, &c, otp::PURPOSE_LOGIN2FA).await {
                    // Use the same email_lower key that otp::create used, not a
                    // re-derivation via to_lowercase(), to guarantee the revoke
                    // targets the right row even on data-inconsistency edge cases.
                    if let Err(re) = blocking(st.pool.clone(), move |conn| {
                        otp::revoke(conn, otp::PURPOSE_LOGIN2FA, &email_lower)
                    })
                    .await
                    {
                        tracing::warn!(error = %re, "failed to roll back undelivered 2FA OTP");
                    }
                    return Err(e);
                }
            }
            Ok(Json(
                json!({ "ok": true, "pending": "2fa", "email": email }),
            ))
        }
    }
}

#[derive(Deserialize)]
struct LoginVerifyReq {
    handle: String,
    code: String,
    phone_id: Option<String>,
}

/// Login step 2: verify the emailed 2FA code and mint the session.
async fn login_verify(
    State(st): State<AppState>,
    Json(req): Json<LoginVerifyReq>,
) -> Result<Json<Value>, ApiError> {
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let v = finish_otp_session(
            &tx,
            otp::PURPOSE_LOGIN2FA,
            &req.handle,
            &req.code,
            None,
            req.phone_id.as_deref(),
        )?;
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct RecoverStartReq {
    handle: String,
}

/// Recovery step 1: if the account has a verified email, email a code. Always
/// returns `ok` (never reveals whether the handle exists).
async fn recover_start(
    State(st): State<AppState>,
    Json(req): Json<RecoverStartReq>,
) -> Result<Json<Value>, ApiError> {
    if !st.email_enabled() {
        return Ok(Json(
            json!({ "ok": false, "error": "recovery_unavailable" }),
        ));
    }
    let created = blocking(st.pool.clone(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = match auth::account_full_by_handle(&tx, &req.handle)? {
            Some(info) if info.email_verified && info.email_lower.is_some() => {
                let el = info.email_lower.clone().unwrap_or_default();
                match otp::create(
                    &tx,
                    otp::PURPOSE_RECOVERY,
                    &el,
                    Some(&info.account_id),
                    None,
                )? {
                    // Carry `el` (the key used for OTP creation) alongside the
                    // display email so the revoke below uses the identical key.
                    CreateOtp::Issued(code) => {
                        Some((info.email.clone().unwrap_or_default(), el, Some(code)))
                    }
                    CreateOtp::TooSoon { .. } => {
                        Some((info.email.clone().unwrap_or_default(), el, None))
                    }
                }
            }
            _ => None,
        };
        tx.commit()?;
        Ok::<Option<(String, String, Option<String>)>, ApiError>(out)
    })
    .await?;
    if let Some((email, email_lower, Some(code))) = &created {
        if let Err(e) = st.mailer.send(email, code, otp::PURPOSE_RECOVERY).await {
            // Existence secrecy: recover_start must return an identical response
            // whether or not the handle maps to a verified account (unlike
            // register/login, which may surface 5xx). Propagating the SMTP error
            // would 5xx *only* for real verified accounts, creating an account-
            // enumeration oracle. Log it, roll back the undelivered code (so the
            // resend cooldown doesn't strand a real user), then fall through to
            // the uniform ok response.
            tracing::warn!(error = %e, "recovery OTP email send failed (suppressed for existence secrecy)");
            let el = email_lower.clone();
            if let Err(re) = blocking(st.pool.clone(), move |conn| {
                otp::revoke(conn, otp::PURPOSE_RECOVERY, &el)
            })
            .await
            {
                tracing::warn!(error = %re, "failed to roll back undelivered recovery OTP");
            }
        }
    }
    Ok(Json(json!({ "ok": true, "sent": true })))
}

#[derive(Deserialize)]
struct RecoverVerifyReq {
    handle: String,
    code: String,
    new_pin: String,
    phone_id: Option<String>,
}

/// Recovery step 2: verify the emailed code, set a new PIN, and mint a session.
async fn recover_verify(
    State(st): State<AppState>,
    Json(req): Json<RecoverVerifyReq>,
) -> Result<Json<Value>, ApiError> {
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let v = finish_otp_session(
            &tx,
            otp::PURPOSE_RECOVERY,
            &req.handle,
            &req.code,
            Some(req.new_pin.as_str()),
            req.phone_id.as_deref(),
        )?;
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

/// Shared tail for login-2FA and recovery: resolve the account by handle, verify
/// the OTP for `purpose`, optionally set a new PIN, and mint a session.
fn finish_otp_session(
    tx: &rusqlite::Transaction<'_>,
    purpose: &str,
    handle: &str,
    code: &str,
    new_pin: Option<&str>,
    phone_id: Option<&str>,
) -> Result<Value, ApiError> {
    let info = match auth::account_full_by_handle(tx, handle)? {
        Some(i) => i,
        None => return Ok(json!({ "ok": false, "error": "invalid_code" })),
    };
    let el = match info.email_lower.clone() {
        Some(e) => e,
        None => return Ok(json!({ "ok": false, "error": "invalid_code" })),
    };
    // Validate the new PIN *before* consuming the OTP. auth::set_pin() only
    // rejects on an invalid PIN format, so a bad PIN must not burn the code and
    // strand the user behind the resend cooldown.
    if let Some(pin) = new_pin {
        if !auth::valid_pin(pin) {
            return Ok(json!({ "ok": false, "error": "bad_pin" }));
        }
    }
    match otp::verify(tx, purpose, &el, code)? {
        VerifyOtp::Ok { account_id, .. } => {
            if account_id.as_deref() != Some(info.account_id.as_str()) {
                return Ok(json!({ "ok": false, "error": "invalid_code" }));
            }
            if let Some(pin) = new_pin {
                // valid_pin was already checked before the OTP was consumed, so a
                // false here means set_pin rejected for some *other* reason — a
                // server-side inconsistency, not a user input error. Surface it as
                // an internal error so the tx rolls back (OTP not burned) and the
                // user can retry, mirroring register_verify's server-fault path.
                if !auth::set_pin(tx, &info.account_id, pin)? {
                    return Err(ApiError::internal(
                        "set_pin rejected an already-validated PIN",
                    ));
                }
            }
            let token = auth::create_session(tx, &info.account_id, phone_id)?;
            Ok(json!({ "ok": true, "session": token,
                "account": { "account_id": info.account_id, "handle": info.handle, "display_name": info.display_name } }))
        }
        VerifyOtp::Invalid => Ok(json!({ "ok": false, "error": "invalid_code" })),
    }
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

/// The signed-in account. No `linked_mc`: character ownership is not a stored
/// property of an account any more (schema v5), and reporting the last-charged
/// UUID as "your linked character" would state a relationship nothing maintains.
async fn me(State(st): State<AppState>, acct: AuthedAccount) -> Result<Json<Value>, ApiError> {
    let id = acct.account_id;
    let info = blocking(st.pool, move |conn| {
        auth::account_full(conn, &id)?
            .ok_or_else(|| ApiError::unauthorized("account no longer exists"))
    })
    .await?;
    Ok(Json(json!({
        "ok": true,
        "account": { "account_id": info.account_id, "handle": info.handle, "display_name": info.display_name },
        "email": info.email,
        "email_verified": info.email_verified,
    })))
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

/// The character's chargeable inventory. Takes **no arguments**: the character is
/// whichever one this account last confirmed via `/wallet/attest/session`.
///
/// This endpoint deliberately never triggers an attestation itself. Consent is a
/// user action, and an inventory read is something the app does on its own
/// initiative (opening a tab, refreshing after a charge) — wiring a modal to it
/// would make the phone ask for approval at moments the user did not initiate.
/// Instead it reports `attestation_required` and lets the app offer the button.
async fn inventory(
    State(st): State<AppState>,
    acct: AuthedAccount,
) -> Result<Json<Value>, ApiError> {
    if !st.can_charge() {
        return Ok(Json(json!({
            "ok": false, "error": "mc_unavailable", "can_charge": false,
            "emeralds": 0, "blocks": 0, "chargeable": 0,
        })));
    }
    let Some(session) = st.char_sessions.get(&acct.account_id, now_ms()) else {
        return Ok(Json(json!({
            "ok": false, "error": "attestation_required", "can_charge": true,
            "emeralds": 0, "blocks": 0, "chargeable": 0,
        })));
    };
    let inv = st
        .charge
        .query_inventory(&session.attester_id, &session.mc_uuid)
        .await?;
    // Surface the real outcome instead of a misleading 0 (CLAUDE.md: no symptom
    // hiding). `reachable=false` ⇒ that server's mod never answered.
    if !inv.reachable {
        return Ok(Json(json!({
            "ok": false, "error": "character_unreachable", "can_charge": true,
            "emeralds": 0, "blocks": 0, "chargeable": 0,
        })));
    }
    if !inv.online {
        // The mod answered "that UUID is nobody here". Our cached route is what
        // went stale — typically the player moved to another server — so drop it
        // and ask for a fresh confirmation. Reporting "you are not logged in"
        // would state as fact something we no longer have any basis for.
        st.char_sessions.invalidate(&acct.account_id);
        return Ok(Json(json!({
            "ok": false, "error": "attestation_required", "can_charge": true,
            "emeralds": 0, "blocks": 0, "chargeable": 0,
        })));
    }
    Ok(Json(json!({
        "ok": true, "can_charge": true,
        "emeralds": inv.emeralds, "blocks": inv.blocks, "chargeable": inv.chargeable,
    })))
}

// ── host attestation ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ChallengeReq {
    purpose: String,
}

/// Issue a single-use nonce for one attestation, bound to this account and to
/// what the assertion will be allowed to authorize.
async fn attest_challenge(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<ChallengeReq>,
) -> Result<Json<Value>, ApiError> {
    let Some(purpose) = AttestPurpose::parse(&req.purpose) else {
        return Ok(Json(json!({ "ok": false, "error": "bad_purpose" })));
    };
    let (challenge, expires_unix_ms) =
        st.challenges
            .issue(&acct.account_id, purpose.as_str(), now_ms());
    Ok(Json(json!({
        "ok": true,
        "challenge": challenge,
        "purpose": purpose.as_str(),
        "expires_unix_ms": expires_unix_ms,
    })))
}

#[derive(Deserialize)]
struct AttestSessionReq {
    assertion: String,
}

/// Confirm which character this account is playing, so inventory reads have
/// something to address.
///
/// A confirmation is NOT authorization to consume: a charge always carries its
/// own assertion, bound to its own amount and idem_key.
async fn attest_session(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<AttestSessionReq>,
) -> Result<Json<Value>, ApiError> {
    // A confirmation binds to a constant hash: it has no request content of its
    // own, and the replay protection is the challenge, which is inside the signed
    // claims and spent exactly once below. The domain still separates it from the
    // charge hash space, so a charge assertion cannot be presented here.
    let facts = match st
        .attest
        .verify(
            &req.assertion,
            &attest::session_request_hash(),
            attest::now_unix_secs(),
        )
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "attest/session: assertion refused");
            return Ok(Json(json!({ "ok": false, "error": e.code() })));
        }
    };
    if !st.challenges.consume(
        facts.challenge(),
        &acct.account_id,
        AttestPurpose::Session.as_str(),
        now_ms(),
    ) {
        return Ok(Json(json!({ "ok": false, "error": "attest_challenge" })));
    }
    let Some(mc_uuid) = identity::normalize_uuid(facts.subject()) else {
        tracing::error!(subject = %facts.subject(),
            "attest/session: signed subject is not a UUID — refusing rather than routing to it");
        return Ok(Json(json!({ "ok": false, "error": "attest_invalid" })));
    };
    st.char_sessions
        .put(&acct.account_id, &mc_uuid, facts.attester_id(), now_ms());
    log_accepted_assertion("attest/session", &acct.account_id, &facts);
    Ok(Json(json!({
        "ok": true,
        "mc_uuid": mc_uuid,
        "expires_unix_ms": now_ms() + attest::CHAR_SESSION_TTL_MS,
    })))
}

/// Record an accepted assertion.
///
/// `mochi_account` is the account the OS redeemed the credential under. It is
/// logged so one request can be traced across the phone, the Hub and this
/// backend — it is deliberately NOT what `account` says, and no code path reads
/// it for a decision. `iat`/`exp` make a clock skew against the Hub (which would
/// otherwise present as every assertion being `attest_invalid`) visible here.
fn log_accepted_assertion(path: &str, account_id: &str, facts: &attest::AttestedFacts) {
    tracing::info!(
        path,
        account = %account_id,
        attester_id = %facts.attester_id(),
        mochi_account = %facts.account_id(),
        iat = facts.iat(),
        exp = facts.exp(),
        "host attestation accepted"
    );
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
    /// Present once [`riskauth`] asks for it — the client posts without one,
    /// reads `pin_required` and asks the user. Small everyday sends still go
    /// through untouched.
    pin: Option<String>,
    otp: Option<String>,
}

async fn send(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<SendReq>,
) -> Result<Json<Value>, ApiError> {
    if req.idem_key.trim().is_empty() {
        return Err(ApiError::bad_request("idem_key required"));
    }
    let email_enabled = st.email_enabled();
    let backoff = st.pin_backoff.clone();
    let value = blocking(st.pool, move |conn| {
        // The replay is answered first, and outside the money transaction, so a
        // retry of a send that already happened costs no PIN attempt.
        if let Some(prev) = db::idem_get(conn, &req.idem_key, "send")? {
            return Ok(replay(prev));
        }
        let to = match auth::lookup_handle(conn, &req.to_handle)? {
            Some(a) => a,
            None => {
                return Ok::<Value, ApiError>(json!({ "ok": false, "error": "unknown_target" }))
            }
        };
        let caller = riskauth::Caller {
            account_id: &acct.account_id,
            phone_id: acct.phone_id.as_deref(),
            session_key: &acct.session_key,
            pin: req.pin.as_deref(),
            otp: req.otp.as_deref(),
        };
        let ticket = match riskauth::step_up(
            conn,
            &backoff,
            email_enabled,
            &caller,
            req.amount,
            Requirement::None,
        )? {
            riskauth::StepUp::Cleared(t) => t,
            riskauth::StepUp::Refused(v) => return Ok(v),
        };

        // Single BEGIN IMMEDIATE: idem check-reserve-execute-record is one atomic
        // unit, so concurrent retries of the same idem_key serialize and the
        // second one replays (no TOCTOU double-spend).
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(prev) = db::idem_get(&tx, &req.idem_key, "send")? {
            drop(tx);
            riskauth::refund_attempt(conn, &acct.account_id, &ticket)?;
            return Ok(replay(prev));
        }
        if let Some(refused) = riskauth::settle(&tx, &acct.account_id, &ticket, now_ms())? {
            // The PIN itself was right — a wrong emailed code has its own attempt
            // counter and must not also eat into the PIN's. (Epoch-guarded, so
            // this is a no-op when a concurrent attempt owns the counter.)
            drop(tx);
            riskauth::refund_attempt(conn, &acct.account_id, &ticket)?;
            return Ok(refused);
        }
        let label = format!("@{} へ送金", to.handle);
        let result = wallet::transfer(
            &tx,
            &acct.account_id,
            &to.account_id,
            req.amount,
            "send",
            &label,
        )?;
        let (v, ok) = tx_result_json(result);
        if !ok {
            // Nothing moved, so the attempt the correct PIN spent goes back.
            drop(tx);
            riskauth::refund_attempt(conn, &acct.account_id, &ticket)?;
            return Ok(v);
        }
        db::idem_put(&tx, &req.idem_key, "send", &v.to_string())?;
        tx.commit()?;
        Ok(v)
    })
    .await?;
    Ok(Json(value))
}

// ── step-up second factor ────────────────────────────────────────────────────

/// Mail a step-up code to this account's verified address.
///
/// Its own OTP purpose, so a code the user asked for to confirm a large payment
/// cannot be replayed as a login, or the other way round.
async fn stepup_otp(
    State(st): State<AppState>,
    acct: AuthedAccount,
) -> Result<Json<Value>, ApiError> {
    if !st.email_enabled() {
        return Ok(Json(json!({ "ok": false, "error": "otp_unavailable" })));
    }
    let id = acct.account_id.clone();
    let created = blocking(st.pool.clone(), move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let info = auth::account_full(&tx, &id)?
            .ok_or_else(|| ApiError::unauthorized("account no longer exists"))?;
        let out = match info.email_lower.clone().filter(|_| info.email_verified) {
            Some(el) => match otp::create(&tx, riskauth::PURPOSE_STEPUP, &el, Some(&id), None)? {
                CreateOtp::Issued(code) => {
                    Some((info.email.unwrap_or_default(), el, Some(code)))
                }
                CreateOtp::TooSoon { retry_after_ms } => {
                    tx.commit()?;
                    return Ok::<_, ApiError>(Err(retry_after_ms));
                }
            },
            None => None,
        };
        tx.commit()?;
        Ok(Ok(out))
    })
    .await?;
    let created = match created {
        Ok(v) => v,
        Err(retry_after_ms) => {
            return Ok(Json(
                json!({ "ok": false, "error": "too_soon", "retry_after_ms": retry_after_ms }),
            ))
        }
    };
    let Some((email, email_lower, Some(code))) = created else {
        return Ok(Json(json!({ "ok": false, "error": "otp_unavailable" })));
    };
    if let Err(e) = st.mailer.send(&email, &code, riskauth::PURPOSE_STEPUP).await {
        // Roll the code back so the resend cooldown does not strand a user behind
        // a code that was never delivered (the register/login discipline).
        if let Err(re) = blocking(st.pool.clone(), move |conn| {
            otp::revoke(conn, riskauth::PURPOSE_STEPUP, &email_lower)
        })
        .await
        {
            tracing::warn!(error = %re, "failed to roll back undelivered step-up OTP");
        }
        return Err(e);
    }
    Ok(Json(json!({ "ok": true, "sent": true, "email": email })))
}

// ── EC payment, payer side ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct IntentQuery {
    intent_id: String,
}

/// Everything the approval screen may show. Deliberately the only source: what
/// the launching app passed along is an `intent_id` and nothing else.
async fn payment_intent(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Query(q): Query<IntentQuery>,
) -> Result<Json<Value>, ApiError> {
    let v = blocking(st.pool, move |conn| {
        let Some(intent) = payments::get(conn, &q.intent_id)? else {
            return Ok::<Value, ApiError>(json!({ "ok": false, "error": "unknown_intent" }));
        };
        // An intent addressed to somebody else is not this account's to look at.
        if let Some(hint) = &intent.payer_hint_account_id {
            if hint != &acct.account_id {
                return Ok(json!({ "ok": false, "error": "payer_mismatch" }));
            }
        }
        Ok(json!({ "ok": true, "intent": payments::payer_view(conn, &intent, now_ms())? }))
    })
    .await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct ApproveReq {
    intent_id: String,
    /// Optional in the wire schema so a client that has not collected one yet
    /// gets `{ok:false,error:"pin_required"}` — a domain answer it can act on —
    /// rather than a deserialization rejection.
    pin: Option<String>,
    otp: Option<String>,
}

async fn payment_approve(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<ApproveReq>,
) -> Result<Json<Value>, ApiError> {
    let email_enabled = st.email_enabled();
    let backoff = st.pin_backoff.clone();
    let value = blocking(st.pool, move |conn| {
        payments::approve(
            conn,
            &backoff,
            email_enabled,
            &riskauth::Caller {
                account_id: &acct.account_id,
                phone_id: acct.phone_id.as_deref(),
                session_key: &acct.session_key,
                pin: req.pin.as_deref(),
                otp: req.otp.as_deref(),
            },
            &req.intent_id,
        )
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct DeclineReq {
    intent_id: String,
}

async fn payment_decline(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<DeclineReq>,
) -> Result<Json<Value>, ApiError> {
    let value = blocking(st.pool, move |conn| {
        payments::decline(conn, &req.intent_id, &acct.account_id).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(value))
}

// ── merchant portal (session + PIN) ──────────────────────────────────────────

/// Re-authenticate the portal caller with their PIN.
///
/// Stage 1 and 2 of the [`crate::auth`] split, without the money stage: nothing
/// under `/merchant/portal/*` moves a balance, so there is no transaction to
/// settle into and the counter is cleared on the spot.
async fn portal_pin(
    st: &AppState,
    acct: &AuthedAccount,
    pin: &str,
    policy: LockoutPolicy,
) -> Result<Option<Value>, ApiError> {
    let now = now_ms();
    if let Err(retry_after_ms) = st.pin_backoff.check(&acct.session_key, now) {
        return Ok(Some(
            json!({ "ok": false, "error": "too_many_attempts", "retry_after_ms": retry_after_ms }),
        ));
    }
    let id = acct.account_id.clone();
    let attempt = blocking(st.pool.clone(), move |conn| {
        auth::begin_pin_attempt(conn, &id, policy)
    })
    .await?;
    let (pin_hash, epoch) = match attempt {
        PinAttempt::Ready { pin_hash, epoch } => (pin_hash, epoch),
        PinAttempt::Locked { retry_after_ms } => {
            return Ok(Some(
                json!({ "ok": false, "error": "locked", "retry_after_ms": retry_after_ms }),
            ))
        }
        PinAttempt::NoPin => {
            return Ok(Some(json!({ "ok": false, "error": "invalid_pin" })));
        }
    };
    let pin = pin.to_string();
    let ok = tokio::task::spawn_blocking(move || auth::verify_pin_hash(&pin, &pin_hash)).await?;
    if !ok {
        let retry_after_ms = st.pin_backoff.record_failure(&acct.session_key, now);
        return Ok(Some(
            json!({ "ok": false, "error": "invalid_pin", "retry_after_ms": retry_after_ms }),
        ));
    }
    st.pin_backoff.clear(&acct.session_key);
    let id = acct.account_id.clone();
    blocking(st.pool.clone(), move |conn| {
        auth::clear_pin_failures(conn, &id, epoch).map_err(ApiError::from)
    })
    .await?;
    Ok(None)
}

#[derive(Deserialize)]
struct PortalRegisterReq {
    name: String,
    sub: Option<String>,
    glyph: Option<String>,
    pal: Option<String>,
    pin: String,
}

async fn portal_register(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<PortalRegisterReq>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if let Err(retry_after_ms) = st.rate.check(
        &format!("mreg:{}", acct.account_id),
        merchant::RL_REGISTER.0,
        merchant::RL_REGISTER.1,
        now_ms(),
    ) {
        return Ok(rate_limited(retry_after_ms));
    }
    if let Some(refused) = portal_pin(&st, &acct, &req.pin, LockoutPolicy::Enforce).await? {
        return Ok(ok_json(refused));
    }
    let email_enabled = st.email_enabled();
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Where mail works, a shop that can take strangers' money must be
        // reachable afterwards. Where it does not (no identity token), the wallet
        // already runs handle+PIN only and this degrades with it rather than
        // making registration impossible.
        if email_enabled {
            let info = auth::account_full(&tx, &acct.account_id)?
                .ok_or_else(|| ApiError::unauthorized("account no longer exists"))?;
            if !info.email_verified {
                return Ok::<Value, ApiError>(
                    json!({ "ok": false, "error": "email_verification_required" }),
                );
            }
        }
        let out = merchant::register(
            &tx,
            &acct.account_id,
            &req.name,
            req.sub.as_deref(),
            req.glyph.as_deref(),
            req.pal.as_deref(),
        )?;
        let v = match out {
            merchant::RegisterOutcome::Ok {
                merchant_id,
                name,
                api_key,
                api_key_prefix,
            } => json!({
                "ok": true, "merchant_id": merchant_id, "name": name,
                // Shown once. Nothing stores it, so nothing can show it again.
                "api_key": api_key, "api_key_prefix": api_key_prefix,
            }),
            merchant::RegisterOutcome::BadName(e) => {
                json!({ "ok": false, "error": "bad_name", "reason": e.code() })
            }
            merchant::RegisterOutcome::BadSub(e) => {
                json!({ "ok": false, "error": "bad_sub", "reason": e.code() })
            }
            merchant::RegisterOutcome::NameTaken => json!({ "ok": false, "error": "name_taken" }),
            merchant::RegisterOutcome::TooManyMerchants => {
                json!({ "ok": false, "error": "too_many_merchants", "limit": merchant::MAX_ACTIVE_MERCHANTS })
            }
        };
        tx.commit()?;
        Ok(v)
    })
    .await?;
    Ok(ok_json(value))
}

#[derive(Deserialize)]
struct PortalKeyReq {
    merchant_id: String,
    pin: String,
}

async fn portal_rotate_key(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<PortalKeyReq>,
) -> Result<Json<Value>, ApiError> {
    if let Some(refused) = portal_pin(&st, &acct, &req.pin, LockoutPolicy::Enforce).await? {
        return Ok(Json(refused));
    }
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let v = match merchant::rotate_key(&tx, &req.merchant_id, &acct.account_id)? {
            Some((api_key, api_key_prefix)) => {
                json!({ "ok": true, "api_key": api_key, "api_key_prefix": api_key_prefix })
            }
            None => json!({ "ok": false, "error": "unknown_merchant" }),
        };
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct PortalStatusReq {
    merchant_id: String,
    status: String,
    pin: String,
}

async fn portal_set_status(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<PortalStatusReq>,
) -> Result<Json<Value>, ApiError> {
    let status = match req.status.as_str() {
        merchant::STATUS_ACTIVE => merchant::STATUS_ACTIVE,
        merchant::STATUS_DISABLED => merchant::STATUS_DISABLED,
        _ => return Ok(Json(json!({ "ok": false, "error": "bad_status" }))),
    };
    // Stopping a shop is the one operation a lockout may not block. Somebody
    // whose API key is being abused, fumbling their PIN under pressure, must not
    // find that the switch which stops the bleeding is the thing they locked
    // themselves out of. The PIN is still required and the failure is still
    // recorded — and nothing here moves money.
    let policy = if status == merchant::STATUS_DISABLED {
        LockoutPolicy::Bypass
    } else {
        LockoutPolicy::Enforce
    };
    if let Some(refused) = portal_pin(&st, &acct, &req.pin, policy).await? {
        return Ok(Json(refused));
    }
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let v = if merchant::set_status(&tx, &req.merchant_id, &acct.account_id, status)? {
            json!({ "ok": true, "merchant_id": req.merchant_id, "status": status })
        } else {
            json!({ "ok": false, "error": "unknown_merchant" })
        };
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct PortalLimitsReq {
    merchant_id: String,
    pin: String,
    max_open_intents: Option<i64>,
    daily_issue_cap: Option<i64>,
}

async fn portal_set_limits(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<PortalLimitsReq>,
) -> Result<Json<Value>, ApiError> {
    if let Some(refused) = portal_pin(&st, &acct, &req.pin, LockoutPolicy::Enforce).await? {
        return Ok(Json(refused));
    }
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let v = if merchant::set_limits(
            &tx,
            &req.merchant_id,
            &acct.account_id,
            req.max_open_intents,
            req.daily_issue_cap,
        )? {
            let m = merchant::get(&tx, &req.merchant_id)?;
            json!({
                "ok": true,
                "max_open_intents": m.as_ref().map(|m| m.max_open_intents),
                "daily_issue_cap": m.map(|m| m.daily_issue_cap),
            })
        } else {
            json!({ "ok": false, "error": "unknown_merchant" })
        };
        tx.commit()?;
        Ok::<Value, ApiError>(v)
    })
    .await?;
    Ok(Json(value))
}

async fn portal_list(
    State(st): State<AppState>,
    acct: AuthedAccount,
) -> Result<Json<Value>, ApiError> {
    let list = blocking(st.pool, move |conn| {
        merchant::list_for_owner(conn, &acct.account_id).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(json!({ "ok": true, "merchants": list })))
}

// ── merchant API (Bearer moy_sk_…) ───────────────────────────────────────────

#[derive(Deserialize)]
struct IntentCreateReq {
    idem_key: String,
    amount: i64,
    description: String,
    order_ref: Option<String>,
    launch_app_id: Option<String>,
    /// Who the shop expects to pay. A hint, and a restriction: naming somebody
    /// stops anyone else from approving, but it never makes them pay.
    payer_hint_handle: Option<String>,
    expires_in_secs: Option<i64>,
}

async fn intent_create(
    State(st): State<AppState>,
    m: MerchantAuth,
    Json(req): Json<IntentCreateReq>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if req.idem_key.trim().is_empty() {
        return Err(ApiError::bad_request("idem_key required"));
    }
    if let Err(retry_after_ms) = st.rate.check(
        &format!("mint:{}", m.merchant.merchant_id),
        merchant::RL_INTENT_CREATE.0,
        merchant::RL_INTENT_CREATE.1,
        now_ms(),
    ) {
        return Ok(rate_limited(retry_after_ms));
    }
    let value = blocking(st.pool, move |conn| {
        let scope = payments::intent_scope(&m.merchant.merchant_id);
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(prev) = db::idem_get(&tx, &req.idem_key, &scope)? {
            return Ok(replay(prev));
        }
        let payer_hint = match req.payer_hint_handle.as_deref().map(str::trim) {
            Some(h) if !h.is_empty() => match auth::lookup_handle(&tx, h)? {
                Some(a) => Some(a.account_id),
                // Refused rather than silently issued to nobody: a shop that
                // meant to address a customer should learn it got the handle
                // wrong before the customer is standing at the till.
                None => {
                    return Ok::<Value, ApiError>(
                        json!({ "ok": false, "error": "unknown_payer_hint" }),
                    )
                }
            },
            _ => None,
        };
        let out = payments::create(
            &tx,
            &m.merchant,
            &NewIntent {
                idem_key: &req.idem_key,
                amount: req.amount,
                description: &req.description,
                order_ref: req.order_ref.as_deref(),
                launch_app_id: req.launch_app_id.as_deref(),
                payer_hint_account_id: payer_hint.as_deref(),
                expires_in_secs: req.expires_in_secs,
            },
        )?;
        let v = match out {
            CreateOutcome::Ok(i) => {
                let v = json!({
                    "ok": true, "intent_id": i.intent_id, "state": i.state,
                    "amount": i.amount, "expires_unix_ms": i.expires_unix_ms,
                });
                db::idem_put(&tx, &req.idem_key, &scope, &v.to_string())?;
                v
            }
            CreateOutcome::BadAmount => json!({ "ok": false, "error": "bad_amount" }),
            CreateOutcome::BadDescription(e) => {
                json!({ "ok": false, "error": "bad_description", "reason": e.code() })
            }
            CreateOutcome::BadOrderRef(e) => {
                json!({ "ok": false, "error": "bad_order_ref", "reason": e.code() })
            }
            CreateOutcome::BadTtl => json!({
                "ok": false, "error": "bad_expires_in_secs",
                "min": payments::MIN_TTL_SECS, "max": payments::MAX_TTL_SECS,
            }),
            CreateOutcome::Capped(IssueGuard::TooManyOpen { limit }) => {
                json!({ "ok": false, "error": "too_many_open_intents", "limit": limit })
            }
            CreateOutcome::Capped(IssueGuard::DailyCapExceeded { limit, issued }) => {
                json!({ "ok": false, "error": "daily_issue_cap", "limit": limit, "issued": issued })
            }
            CreateOutcome::Capped(IssueGuard::Ok) => {
                return Err(ApiError::internal("issuance guard reported Ok as a refusal"))
            }
        };
        tx.commit()?;
        Ok(v)
    })
    .await?;
    Ok(ok_json(value))
}

async fn intent_get(
    State(st): State<AppState>,
    m: MerchantAuth,
    Query(q): Query<IntentQuery>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if let Err(retry_after_ms) = st.rate.check(
        &format!("mget:{}", m.merchant.merchant_id),
        merchant::RL_INTENT_READ.0,
        merchant::RL_INTENT_READ.1,
        now_ms(),
    ) {
        return Ok(rate_limited(retry_after_ms));
    }
    let value = blocking(st.pool, move |conn| {
        match payments::get(conn, &q.intent_id)? {
            // Another merchant's intent reads as a missing one — the same
            // ownership discipline `op_status` uses, so this cannot be turned
            // into an oracle for other shops' order flow.
            Some(i) if i.merchant_id == m.merchant.merchant_id => Ok::<Value, ApiError>(
                json!({ "ok": true, "intent": payments::merchant_view(&m.merchant, &i, now_ms())? }),
            ),
            _ => Ok(json!({ "ok": false, "error": "unknown_intent" })),
        }
    })
    .await?;
    Ok(ok_json(value))
}

#[derive(Deserialize)]
struct IntentCancelReq {
    intent_id: String,
}

async fn intent_cancel(
    State(st): State<AppState>,
    m: MerchantAuth,
    Json(req): Json<IntentCancelReq>,
) -> Result<Json<Value>, ApiError> {
    let value = blocking(st.pool, move |conn| {
        payments::cancel(conn, &req.intent_id, &m.merchant.merchant_id).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(value))
}

#[derive(Deserialize)]
struct ChargeReq {
    idem_key: String,
    amount: i64,
    /// Absent on the first attempt: the app posts without one, and only produces
    /// an assertion if the answer is `attestation_required`. That ordering is
    /// what lets a retry replay silently instead of raising a second consent
    /// modal for a charge the user already approved.
    assertion: Option<String>,
}

/// Consume in-world emeralds and credit this account.
///
/// The account is the SESSION's, always. The assertion decides only which
/// character's emeralds are consumed and on which server — it is never an
/// alternative way to name the account being credited, and `claims.account_id`
/// (a Mochi account, a different namespace entirely) is not read here at all.
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
    // A replay cannot consume anything new, so it needs no fresh consent.
    if let Some(prev) = st
        .charge
        .replay_charge(&acct.account_id, &req.idem_key)
        .await?
    {
        return Ok(Json(prev));
    }
    let Some(assertion) = req.assertion.as_deref().filter(|a| !a.trim().is_empty()) else {
        return Ok(Json(
            json!({ "ok": false, "error": "attestation_required" }),
        ));
    };
    // Bound to THIS charge: a different amount or idem_key hashes differently, so
    // an assertion approved for one consume cannot be spent on another.
    let facts = match st
        .attest
        .verify(
            assertion,
            &attest::charge_request_hash(&req.idem_key, req.amount),
            attest::now_unix_secs(),
        )
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "charge: assertion refused");
            return Ok(Json(json!({ "ok": false, "error": e.code() })));
        }
    };
    if !st.challenges.consume(
        facts.challenge(),
        &acct.account_id,
        AttestPurpose::Charge.as_str(),
        now_ms(),
    ) {
        return Ok(Json(json!({ "ok": false, "error": "attest_challenge" })));
    }
    let Some(mc_uuid) = identity::normalize_uuid(facts.subject()) else {
        tracing::error!(subject = %facts.subject(),
            "charge: signed subject is not a UUID — refusing rather than routing to it");
        return Ok(Json(json!({ "ok": false, "error": "attest_invalid" })));
    };
    log_accepted_assertion("wallet/charge", &acct.account_id, &facts);
    let value = st
        .charge
        .begin_charge(
            &req.idem_key,
            &acct.account_id,
            &mc_uuid,
            facts.attester_id(),
            req.amount,
        )
        .await?;
    // A charge carries the same (character, server) pair a session confirmation
    // would, freshly consented to, so record it: without this the user would
    // approve a charge and then immediately be asked to confirm the character
    // again just to see the resulting inventory.
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        st.char_sessions
            .put(&acct.account_id, &mc_uuid, facts.attester_id(), now_ms());
    }
    Ok(Json(value))
}

#[derive(Deserialize)]
struct WithdrawReq {
    idem_key: String,
    amount: i64,
    /// Absent on the first attempt, exactly as on a charge — the app only
    /// produces an assertion once the answer is `attestation_required`, so a
    /// retry replays instead of raising a second consent modal.
    assertion: Option<String>,
    /// Collected after [`riskauth`] asks. Requested AFTER the assertion, so the
    /// client's second post already carries one and only one Argon2 comparison
    /// happens; the assertion's challenge is not spent until it verifies, so
    /// re-posting it alongside the PIN is fine.
    pin: Option<String>,
    otp: Option<String>,
}

/// Pay eme back out as in-world emeralds.
///
/// The account is the SESSION's, always. The assertion decides only which
/// character receives the emeralds and on which server; it cannot name the wallet
/// that pays, and it cannot start a withdrawal on its own.
async fn withdraw(
    State(st): State<AppState>,
    acct: AuthedAccount,
    Json(req): Json<WithdrawReq>,
) -> Result<Json<Value>, ApiError> {
    if req.idem_key.trim().is_empty() {
        return Err(ApiError::bad_request("idem_key required"));
    }
    if !st.can_charge() {
        return Ok(Json(json!({ "ok": false, "error": "mc_unavailable" })));
    }
    // A replay cannot debit again, so it needs no fresh consent — and asking for
    // it would prompt the user to approve a payout that is already in flight.
    if let Some(prev) = st
        .charge
        .replay_withdraw(&acct.account_id, &req.idem_key)
        .await?
    {
        return Ok(Json(prev));
    }
    // Refuse what cannot succeed BEFORE asking the phone for consent: a modal the
    // user approves only to be told "insufficient" also burns the challenge it was
    // issued for. Both checks are advisory — the authoritative ones run inside the
    // reserve transaction, which is the only place that can be raced.
    if req.amount <= 0 || req.amount > wallet::MAX_WITHDRAW_PER_OP {
        return Ok(Json(json!({ "ok": false, "error": "bad_amount" })));
    }
    let id = acct.account_id.clone();
    let balance = blocking(st.pool.clone(), move |conn| {
        wallet::balance(conn, &id).map_err(ApiError::from)
    })
    .await?;
    if balance < req.amount {
        return Ok(Json(
            json!({ "ok": false, "error": "insufficient", "balance": balance }),
        ));
    }
    let Some(assertion) = req.assertion.as_deref().filter(|a| !a.trim().is_empty()) else {
        return Ok(Json(
            json!({ "ok": false, "error": "attestation_required" }),
        ));
    };
    // The shared outflow gate. A withdrawal is the one movement that leaves the
    // wallet system entirely, so it goes through the same door as a send and a
    // payment rather than relying on the attestation — which proves which
    // character receives the emeralds, never that the person at the phone is the
    // account holder.
    //
    // Unlike send and approve, the step-up is settled in a transaction of its
    // own: the reserve happens inside `charge::begin_withdraw`, and this gate is
    // deliberately not reaching into that module's transaction. The cost is that
    // a code is spent even if the reserve then fails — the user asks for another
    // one. Nothing is authorized twice, because the counter is cleared and the
    // code consumed exactly once here.
    {
        let acct = acct.clone();
        let email_enabled = st.email_enabled();
        let backoff = st.pin_backoff.clone();
        let (pin, otp) = (req.pin.clone(), req.otp.clone());
        let amount = req.amount;
        let refused = blocking(st.pool.clone(), move |conn| {
            let caller = riskauth::Caller {
                account_id: &acct.account_id,
                phone_id: acct.phone_id.as_deref(),
                session_key: &acct.session_key,
                pin: pin.as_deref(),
                otp: otp.as_deref(),
            };
            let ticket = match riskauth::step_up(
                conn,
                &backoff,
                email_enabled,
                &caller,
                amount,
                Requirement::None,
            )? {
                riskauth::StepUp::Cleared(t) => t,
                riskauth::StepUp::Refused(v) => return Ok(Some(v)),
            };
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if let Some(v) = riskauth::settle(&tx, &acct.account_id, &ticket, now_ms())? {
                // Same reasoning as on a send: the PIN was correct, so a wrong
                // code does not also spend a PIN attempt.
                drop(tx);
                riskauth::refund_attempt(conn, &acct.account_id, &ticket)?;
                return Ok(Some(v));
            }
            tx.commit()?;
            Ok::<Option<Value>, ApiError>(None)
        })
        .await?;
        if let Some(v) = refused {
            return Ok(Json(v));
        }
    }
    // Bound to THIS withdrawal, under a domain of its own: an assertion approved
    // for a charge does not verify here, and one approved for 100 エメ does not
    // authorize paying out 10000.
    let facts = match st
        .attest
        .verify(
            assertion,
            &attest::withdraw_request_hash(&req.idem_key, req.amount),
            attest::now_unix_secs(),
        )
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "withdraw: assertion refused");
            return Ok(Json(json!({ "ok": false, "error": e.code() })));
        }
    };
    if !st.challenges.consume(
        facts.challenge(),
        &acct.account_id,
        AttestPurpose::Withdraw.as_str(),
        now_ms(),
    ) {
        return Ok(Json(json!({ "ok": false, "error": "attest_challenge" })));
    }
    let Some(mc_uuid) = identity::normalize_uuid(facts.subject()) else {
        tracing::error!(subject = %facts.subject(),
            "withdraw: signed subject is not a UUID — refusing rather than paying out to it");
        return Ok(Json(json!({ "ok": false, "error": "attest_invalid" })));
    };
    log_accepted_assertion("wallet/withdraw", &acct.account_id, &facts);
    let value = st
        .charge
        .begin_withdraw(
            &req.idem_key,
            &acct.account_id,
            &mc_uuid,
            facts.attester_id(),
            req.amount,
        )
        .await?;
    // Same reasoning as on a charge: this carried a freshly consented (character,
    // server) pair, so record it rather than re-prompting for a confirmation just
    // to show the resulting inventory.
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        st.char_sessions
            .put(&acct.account_id, &mc_uuid, facts.attester_id(), now_ms());
    }
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
        return Err(ApiError::forbidden(
            "dev credit disabled (set MOYMOY_DEV_CREDIT=1)",
        ));
    }
    if req.amount <= 0 || req.amount > wallet::MAX_AMOUNT {
        return Err(ApiError::bad_request("bad amount"));
    }
    let value = blocking(st.pool, move |conn| {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let acct = match auth::lookup_handle(&tx, &req.handle)? {
            Some(a) => a,
            None => {
                return Ok::<Value, ApiError>(json!({ "ok": false, "error": "unknown_target" }))
            }
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

/// Too many calls. A `429` and not a `200 {ok:false}`: the wallet is refusing to
/// look at the request at all, which is an infrastructure answer — unlike
/// "insufficient" or "already_paid", which are facts about the request itself.
fn rate_limited(retry_after_ms: i64) -> (StatusCode, Json<Value>) {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "ok": false, "error": "rate_limited", "retry_after_ms": retry_after_ms })),
    )
}

/// The ordinary answer from a handler that can also rate-limit.
fn ok_json(v: Value) -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(v))
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
        Err(e) => {
            tracing::error!(error = %e, "replay: corrupt idempotency record; returning internal_error instead of a fabricated success");
            json!({ "ok": false, "error": "internal_error" })
        }
    }
}
