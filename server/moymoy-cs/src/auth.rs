//! Independent MoyMoy accounts: handle + PIN credentials, Argon2id PIN hashing,
//! and backend-verified sessions. This is what makes identity *verifiable* rather
//! than self-asserted: a request proves who it is by presenting a session token
//! (header `X-MoyMoy-Session`) that this backend minted and stores (only as a
//! SHA-256 hash) — not by claiming an mc_uuid the way the old model did.
//!
//! Security posture (CLAUDE.md):
//!   - PINs are never stored or returned in plaintext — only an Argon2id PHC hash.
//!   - Repeated wrong PINs lock the account for a window (anti-bruteforce).
//!   - Login errors are generic (`invalid_credentials`) so handles can't be
//!     enumerated by probing.
//!   - Session tokens are 256-bit CSPRNG values; the DB holds only their hash and
//!     an expiry, and logout deletes the row.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::api::AppState;
use crate::db::now_ms;
use crate::error::ApiError;
use crate::identity::card_number_for;

/// Session lifetime: 30 days (the app re-logins on expiry).
const SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Wrong-PIN attempts before the account locks.
const MAX_FAILED_ATTEMPTS: i64 = 5;
/// Lockout window after `MAX_FAILED_ATTEMPTS` (15 minutes).
const LOCKOUT_MS: i64 = 15 * 60 * 1000;
/// The session header the app sends (custom `X-*`; passes the runtime
/// FORBIDDEN_HEADERS filter and the gateway untouched — see plan).
pub const SESSION_HEADER: &str = "x-moymoy-session";

/// Public account view (never includes pin_hash / attempts).
#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    pub account_id: String,
    pub handle: String,
    pub display_name: String,
}

/// A freshly minted session: the plaintext token (returned to the client ONCE)
/// plus the account it belongs to.
#[derive(Debug)]
pub struct SessionMint {
    pub token: String,
    pub account: AccountView,
}

/// Outcome of `register` — only `Ok` is a success; the rest are ordinary
/// validation results the handler returns as `200 {ok:false,error}`.
#[derive(Debug)]
pub enum RegisterOutcome {
    Ok(SessionMint),
    BadHandle,
    BadPin,
    BadDisplayName,
    HandleTaken,
}

/// Outcome of `login`. `Invalid` is deliberately generic (no handle-vs-PIN
/// distinction) to prevent account enumeration.
#[derive(Debug)]
pub enum LoginOutcome {
    Ok(SessionMint),
    Invalid,
    Locked { retry_after_ms: i64 },
}

// ── credential helpers ───────────────────────────────────────────────────────

/// Validate a handle (`[A-Za-z0-9_]`, 3–20 chars), returning the trimmed
/// as-entered form (case preserved). Uniqueness/lookup use its lowercase.
pub fn valid_handle(s: &str) -> Option<String> {
    let t = s.trim();
    let len = t.chars().count();
    if (3..=20).contains(&len) && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(t.to_string())
    } else {
        None
    }
}

/// A PIN is 4–6 ASCII digits.
fn valid_pin(pin: &str) -> bool {
    let len = pin.len();
    (4..=6).contains(&len) && pin.chars().all(|c| c.is_ascii_digit())
}

/// Validate a display name (1–24 chars, no control chars), returning it trimmed.
fn valid_display_name(s: &str) -> Option<String> {
    let t = s.trim();
    let len = t.chars().count();
    if (1..=24).contains(&len) && !t.chars().any(|c| c.is_control()) {
        Some(t.to_string())
    } else {
        None
    }
}

/// Argon2id PHC hash of a PIN (embeds a random salt). Returned as a `$argon2id$…`
/// string suitable for storage in `accounts.pin_hash`.
fn hash_pin(pin: &str) -> Result<String, ApiError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pin.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| ApiError::internal(format!("pin hash: {e}")))
}

/// Constant-time-ish PIN verification against a stored PHC hash.
fn verify_pin(pin: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(pin.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A new 256-bit session token, URL-safe base64 (no padding).
fn gen_token() -> String {
    let mut rng = OsRng;
    let mut buf = [0u8; 32];
    rng.fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// SHA-256(token) as base64 — what we persist (never the token itself).
fn token_hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
}

// ── account / session reads ──────────────────────────────────────────────────

/// Public view of an account by id, if it exists and is a login account.
pub fn account_view(conn: &Connection, account_id: &str) -> rusqlite::Result<Option<AccountView>> {
    conn.query_row(
        "SELECT account_id, handle, display_name FROM accounts WHERE account_id = ?1",
        [account_id],
        |r| {
            Ok(AccountView {
                account_id: r.get::<_, String>(0)?,
                handle: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                display_name: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        },
    )
    .optional()
}

/// Resolve a `@handle` (case-insensitive) to a login account view — the send
/// target lookup.
pub fn lookup_handle(conn: &Connection, handle: &str) -> rusqlite::Result<Option<AccountView>> {
    let hl = handle.trim().to_lowercase();
    if hl.is_empty() {
        return Ok(None);
    }
    conn.query_row(
        "SELECT account_id, handle, display_name FROM accounts \
         WHERE handle_lower = ?1 AND pin_hash IS NOT NULL",
        [&hl],
        |r| {
            Ok(AccountView {
                account_id: r.get::<_, String>(0)?,
                handle: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                display_name: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        },
    )
    .optional()
}

/// Mint a session for `account_id`, persisting only the token hash. Returns the
/// plaintext token (shown to the client once).
fn create_session(conn: &Connection, account_id: &str, phone_id: Option<&str>) -> rusqlite::Result<String> {
    let token = gen_token();
    let now = now_ms();
    conn.execute(
        "INSERT INTO moymoy_sessions \
           (session_id, account_id, token_hash, phone_id, created_unix_ms, last_seen_unix_ms, expires_unix_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
        params![
            Uuid::new_v4().to_string(),
            account_id,
            token_hash(&token),
            phone_id,
            now,
            now + SESSION_TTL_MS,
        ],
    )?;
    Ok(token)
}

/// Resolve a presented token to its account_id, if a non-expired session exists.
/// Refreshes `last_seen`. Expired/unknown ⇒ `None`.
pub fn resolve_session(conn: &Connection, token: &str) -> rusqlite::Result<Option<String>> {
    let th = token_hash(token);
    let now = now_ms();
    let row = conn
        .query_row(
            "SELECT account_id, expires_unix_ms FROM moymoy_sessions WHERE token_hash = ?1",
            [&th],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    match row {
        Some((account_id, expires)) if expires > now => {
            conn.execute(
                "UPDATE moymoy_sessions SET last_seen_unix_ms = ?2 WHERE token_hash = ?1",
                params![th, now],
            )?;
            Ok(Some(account_id))
        }
        Some(_) => {
            // Expired — best-effort cleanup so the table doesn't accumulate dead rows.
            if let Err(e) = conn.execute("DELETE FROM moymoy_sessions WHERE token_hash = ?1", [&th]) {
                tracing::debug!(error = %e, "resolve_session: expired-row cleanup failed");
            }
            Ok(None)
        }
        None => Ok(None),
    }
}

/// Revoke a session (logout). No-op if the token is unknown.
pub fn logout(conn: &Connection, token: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM moymoy_sessions WHERE token_hash = ?1",
        [token_hash(token)],
    )?;
    Ok(())
}

// ── register / login ─────────────────────────────────────────────────────────

/// Create a new MoyMoy account (handle + PIN) and mint its first session. The
/// caller owns the (IMMEDIATE) transaction so the uniqueness check + insert are
/// atomic against a concurrent same-handle registration.
pub fn register(
    conn: &Connection,
    handle_input: &str,
    display_input: &str,
    pin: &str,
    phone_id: Option<&str>,
) -> Result<RegisterOutcome, ApiError> {
    let handle = match valid_handle(handle_input) {
        Some(h) => h,
        None => return Ok(RegisterOutcome::BadHandle),
    };
    let display = match valid_display_name(display_input) {
        Some(d) => d,
        None => return Ok(RegisterOutcome::BadDisplayName),
    };
    if !valid_pin(pin) {
        return Ok(RegisterOutcome::BadPin);
    }
    let handle_lower = handle.to_lowercase();
    let taken = conn
        .query_row(
            "SELECT 1 FROM accounts WHERE handle_lower = ?1",
            [&handle_lower],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if taken {
        return Ok(RegisterOutcome::HandleTaken);
    }

    let pin_hash = hash_pin(pin)?;
    let account_id = Uuid::new_v4().to_string();
    let now = now_ms();
    let card = card_number_for(&account_id);
    let holder = display.to_uppercase();
    conn.execute(
        "INSERT INTO accounts \
           (account_id, balance, holder, card_number, is_merchant, \
            handle, handle_lower, display_name, pin_hash, failed_pin_attempts, \
            created_unix_ms, updated_unix_ms) \
         VALUES (?1, 0, ?2, ?3, 0, ?4, ?5, ?6, ?7, 0, ?8, ?8)",
        // card_expiry omitted — the schema DEFAULT '07/29' is the single source of truth.
        params![account_id, holder, card, handle, handle_lower, display, pin_hash, now],
    )?;
    let token = create_session(conn, &account_id, phone_id)?;
    Ok(RegisterOutcome::Ok(SessionMint {
        token,
        account: AccountView {
            account_id,
            handle,
            display_name: display,
        },
    }))
}

/// Authenticate handle + PIN and mint a session. Enforces lockout and returns a
/// generic `Invalid` for any failure mode (unknown handle, wrong PIN).
pub fn login(
    conn: &Connection,
    handle_input: &str,
    pin: &str,
    phone_id: Option<&str>,
) -> Result<LoginOutcome, ApiError> {
    let hl = handle_input.trim().to_lowercase();
    if hl.is_empty() {
        return Ok(LoginOutcome::Invalid);
    }
    let row = conn
        .query_row(
            "SELECT account_id, handle, display_name, pin_hash, failed_pin_attempts, locked_until_unix_ms \
             FROM accounts WHERE handle_lower = ?1",
            [&hl],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                ))
            },
        )
        .optional()?;
    let (account_id, handle, display_name, pin_hash, attempts, locked_until) = match row {
        Some(x) => x,
        None => return Ok(LoginOutcome::Invalid),
    };

    let now = now_ms();
    if let Some(until) = locked_until {
        if until > now {
            return Ok(LoginOutcome::Locked {
                retry_after_ms: until - now,
            });
        }
    }

    let ok = pin_hash.as_deref().map(|h| verify_pin(pin, h)).unwrap_or(false);
    if !ok {
        let new_attempts = attempts + 1;
        let new_lock = if new_attempts >= MAX_FAILED_ATTEMPTS {
            Some(now + LOCKOUT_MS)
        } else {
            None
        };
        conn.execute(
            "UPDATE accounts SET failed_pin_attempts = ?2, locked_until_unix_ms = ?3, updated_unix_ms = ?4 \
             WHERE account_id = ?1",
            params![account_id, new_attempts, new_lock, now],
        )?;
        if new_lock.is_some() {
            return Ok(LoginOutcome::Locked {
                retry_after_ms: LOCKOUT_MS,
            });
        }
        return Ok(LoginOutcome::Invalid);
    }

    // Success: clear the failure counter and mint a session.
    conn.execute(
        "UPDATE accounts SET failed_pin_attempts = 0, locked_until_unix_ms = NULL, updated_unix_ms = ?2 \
         WHERE account_id = ?1",
        params![account_id, now],
    )?;
    let token = create_session(conn, &account_id, phone_id)?;
    Ok(LoginOutcome::Ok(SessionMint {
        token,
        account: AccountView {
            account_id,
            handle: handle.unwrap_or_default(),
            display_name: display_name.unwrap_or_default(),
        },
    }))
}

// ── extractor ────────────────────────────────────────────────────────────────

/// The authenticated caller, resolved from the `X-MoyMoy-Session` header. Used as
/// an axum extractor on every wallet endpoint; rejects with `401` when the
/// session is missing, unknown, or expired.
#[derive(Debug, Clone)]
pub struct AuthedAccount {
    pub account_id: String,
}

#[axum::async_trait]
impl FromRequestParts<AppState> for AuthedAccount {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ApiError::unauthorized("missing session"))?
            .to_string();
        let pool = state.pool.clone();
        let account_id = tokio::task::spawn_blocking(move || -> Result<Option<String>, ApiError> {
            let conn = pool.get()?;
            resolve_session(&conn, &token).map_err(ApiError::from)
        })
        .await??;
        match account_id {
            Some(account_id) => Ok(AuthedAccount { account_id }),
            None => Err(ApiError::unauthorized("invalid or expired session")),
        }
    }
}
