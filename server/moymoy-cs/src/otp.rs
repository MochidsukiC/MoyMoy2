//! Email one-time-passcodes: signup verification, login 2FA, and PIN recovery.
//!
//! Codes are 6 digits, delivered over SMTP via `mochi-hub-mailer` (all
//! connection params from `MOCHI_SMTP_*` env — nothing hardcoded). We persist
//! only the SHA-256 hash of a code (never the code), with a 10-minute expiry, a
//! 5-attempt limit, single-use semantics, and a per-target resend cooldown.
//!
//! Email features are active only when SMTP is configured; otherwise the wallet
//! degrades to handle+PIN and this module's rows stay unused. A dev-only
//! `MOYMOY_DEV_OTP_LOG=1` mode lets the flow be exercised locally without SMTP
//! (codes go to the log — never in a real deploy).

use std::sync::Arc;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use mochi_hub_mailer::{MailSender, SmtpMailSender};

use crate::db::now_ms;
use crate::error::ApiError;

const OTP_TTL_MS: i64 = 10 * 60 * 1000; // 10 minutes
const OTP_MAX_ATTEMPTS: i64 = 5;
const OTP_RESEND_COOLDOWN_MS: i64 = 60 * 1000; // 1 minute between sends per target

pub const PURPOSE_SIGNUP: &str = "signup";
pub const PURPOSE_LOGIN2FA: &str = "login2fa";
pub const PURPOSE_RECOVERY: &str = "recovery";

/// The pending account data carried in a signup OTP until the email is verified
/// (the real `accounts` row is created only on verification).
#[derive(Debug, Serialize, Deserialize)]
pub struct PendingSignup {
    pub handle: String,
    pub handle_lower: String,
    pub display_name: String,
    pub pin_hash: String,
}

/// Outbound email + the email-features gate. Holds the real SMTP sender when
/// `MOCHI_SMTP_*` is configured; `dev_log` exercises the OTP flow locally
/// without SMTP by logging codes (dev only).
#[derive(Clone)]
pub struct Mailer {
    sender: Option<Arc<SmtpMailSender>>,
    dev_log: bool,
}

impl Mailer {
    /// Build from the environment: real SMTP if `MOCHI_SMTP_*` parses, else a
    /// dev-log fallback when `MOYMOY_DEV_OTP_LOG=1`, else disabled (degrade).
    pub fn from_env() -> Self {
        let dev_log = crate::env_flag("MOYMOY_DEV_OTP_LOG", false);
        match SmtpMailSender::from_env() {
            Ok(s) => {
                tracing::info!("email OTP enabled (SMTP configured)");
                Mailer { sender: Some(Arc::new(s)), dev_log }
            }
            Err(e) => {
                if dev_log {
                    tracing::warn!(reason = %e, "SMTP not configured — DEV OTP LOG mode (codes logged, not emailed)");
                } else {
                    tracing::info!(reason = %e, "email OTP disabled (no SMTP) — wallet runs handle+PIN only");
                }
                Mailer { sender: None, dev_log }
            }
        }
    }

    /// Whether email-backed features (verify / 2FA / recovery) are active.
    pub fn enabled(&self) -> bool {
        self.sender.is_some() || self.dev_log
    }

    /// Deliver `code` to `email`. Never logs the code unless dev-log mode is on.
    pub async fn send(&self, email: &str, code: &str, purpose: &str) -> Result<(), ApiError> {
        if let Some(sender) = &self.sender {
            sender
                .send_otp(email, code)
                .await
                .map_err(|e| ApiError::internal(format!("email send failed: {e}")))
        } else if self.dev_log {
            tracing::warn!(email, purpose, code, "DEV OTP LOG (email sending disabled) — for local testing only");
            Ok(())
        } else {
            Err(ApiError::internal("email is not enabled"))
        }
    }
}

/// Minimal email sanity (one `@`, non-empty local, a dotted domain, no
/// whitespace). Returns the trimmed address.
pub fn valid_email(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() || t.len() > 254 || t.chars().any(char::is_whitespace) {
        return None;
    }
    let parts: Vec<&str> = t.split('@').collect();
    if parts.len() != 2 {
        return None;
    }
    let (local, domain) = (parts[0], parts[1]);
    if local.is_empty()
        || domain.len() < 3
        || !domain.contains('.')
        || domain.starts_with('.')
        || domain.ends_with('.')
    {
        return None;
    }
    Some(t.to_string())
}

fn gen_code() -> String {
    let mut b = [0u8; 4];
    OsRng.fill_bytes(&mut b);
    let n = u32::from_le_bytes(b) % 1_000_000;
    format!("{n:06}")
}

fn code_hash(code: &str) -> String {
    let digest = Sha256::digest(code.as_bytes());
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
}

/// Result of requesting a code.
pub enum CreateOtp {
    /// A fresh code was issued (plaintext, to be delivered).
    Issued(String),
    /// A code was issued too recently for this target.
    TooSoon { retry_after_ms: i64 },
}

/// Issue an OTP for `(purpose, email_lower)`, replacing any prior code for the
/// same target, subject to a resend cooldown. Returns the plaintext code to
/// deliver. Runs in the caller's (blocking) connection/transaction.
pub fn create(
    conn: &Connection,
    purpose: &str,
    email_lower: &str,
    account_id: Option<&str>,
    payload_json: Option<&str>,
) -> Result<CreateOtp, ApiError> {
    let now = now_ms();
    let recent: Option<i64> = conn
        .query_row(
            "SELECT MAX(created_unix_ms) FROM moymoy_otps WHERE purpose = ?1 AND email_lower = ?2",
            params![purpose, email_lower],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();
    if let Some(t) = recent {
        if now - t < OTP_RESEND_COOLDOWN_MS {
            return Ok(CreateOtp::TooSoon {
                retry_after_ms: OTP_RESEND_COOLDOWN_MS - (now - t),
            });
        }
    }
    // Replace any prior codes for this target (only the latest is valid).
    conn.execute(
        "DELETE FROM moymoy_otps WHERE purpose = ?1 AND email_lower = ?2",
        params![purpose, email_lower],
    )?;
    let code = gen_code();
    conn.execute(
        "INSERT INTO moymoy_otps \
           (otp_id, purpose, email_lower, account_id, code_hash, payload_json, attempts, created_unix_ms, expires_unix_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8)",
        params![
            Uuid::new_v4().to_string(),
            purpose,
            email_lower,
            account_id,
            code_hash(&code),
            payload_json,
            now,
            now + OTP_TTL_MS,
        ],
    )?;
    Ok(CreateOtp::Issued(code))
}

/// Result of verifying a code.
pub enum VerifyOtp {
    /// The code matched; the OTP is consumed. Carries the row's linked account
    /// and pending payload (signup).
    Ok {
        account_id: Option<String>,
        payload: Option<String>,
    },
    /// No matching / unexpired code, or the code was wrong.
    Invalid,
}

/// Verify `code` for `(purpose, email_lower)`. On success consumes (deletes) the
/// OTP and returns its account/payload; on failure increments the attempt
/// counter (deleting once the limit is reached). Expired/attempt-exhausted codes
/// verify as `Invalid`.
pub fn verify(
    conn: &Connection,
    purpose: &str,
    email_lower: &str,
    code: &str,
) -> Result<VerifyOtp, ApiError> {
    let now = now_ms();
    let row = conn
        .query_row(
            "SELECT otp_id, account_id, code_hash, payload_json, attempts, expires_unix_ms \
             FROM moymoy_otps WHERE purpose = ?1 AND email_lower = ?2 \
             ORDER BY created_unix_ms DESC LIMIT 1",
            params![purpose, email_lower],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?;
    let (otp_id, account_id, hash, payload, attempts, expires) = match row {
        Some(x) => x,
        None => return Ok(VerifyOtp::Invalid),
    };
    if now > expires || attempts >= OTP_MAX_ATTEMPTS {
        conn.execute("DELETE FROM moymoy_otps WHERE otp_id = ?1", [&otp_id])?;
        return Ok(VerifyOtp::Invalid);
    }
    if code_hash(code) == hash {
        conn.execute("DELETE FROM moymoy_otps WHERE otp_id = ?1", [&otp_id])?;
        Ok(VerifyOtp::Ok { account_id, payload })
    } else {
        let next = attempts + 1;
        if next >= OTP_MAX_ATTEMPTS {
            conn.execute("DELETE FROM moymoy_otps WHERE otp_id = ?1", [&otp_id])?;
        } else {
            conn.execute(
                "UPDATE moymoy_otps SET attempts = ?2 WHERE otp_id = ?1",
                params![otp_id, next],
            )?;
        }
        Ok(VerifyOtp::Invalid)
    }
}
