-- MoyMoy schema v4 — email verification, login 2FA, and PIN recovery.
--
-- Adds an optional VERIFIED email to an account plus a one-time-passcode (OTP)
-- table backing three flows: signup verification, login second factor, and PIN
-- recovery. Email features are active only when SMTP is configured
-- (MOCHI_SMTP_*); otherwise the wallet degrades to handle+PIN and these
-- columns/rows simply stay unused.

ALTER TABLE accounts ADD COLUMN email                  TEXT;    -- verified address (NULL = none)
ALTER TABLE accounts ADD COLUMN email_lower            TEXT;    -- case-insensitive uniqueness / lookup
ALTER TABLE accounts ADD COLUMN email_verified_unix_ms INTEGER; -- when verified (NULL = unverified)

-- One email ↔ one account (anti-throwaway). Partial index so account rows with
-- no email (degrade mode, merchants/system) don't collide on NULL.
CREATE UNIQUE INDEX IF NOT EXISTS idx_accounts_email_lower
    ON accounts (email_lower) WHERE email_lower IS NOT NULL;

-- One-time passcodes. `purpose` ∈ signup | login2fa | recovery.
--   - signup:   account_id NULL; payload_json carries the pending account
--               (handle / handle_lower / display_name / pin_hash) until verified.
--   - login2fa: account_id set (the account mid-login).
--   - recovery: account_id set (the account resetting its PIN).
-- Only the SHA-256 hash of the code is stored (never the code). Single-use,
-- expiring, attempt-limited.
CREATE TABLE IF NOT EXISTS moymoy_otps (
    otp_id          TEXT PRIMARY KEY,               -- UUID v4
    purpose         TEXT NOT NULL,
    email_lower     TEXT NOT NULL,                  -- verification target
    account_id      TEXT REFERENCES accounts (account_id),
    code_hash       TEXT NOT NULL,                  -- SHA-256(code), base64
    payload_json    TEXT,                           -- pending signup data (signup only)
    attempts        INTEGER NOT NULL DEFAULT 0,
    created_unix_ms INTEGER NOT NULL,
    expires_unix_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_otps_purpose_email ON moymoy_otps (purpose, email_lower);
CREATE INDEX IF NOT EXISTS idx_otps_account ON moymoy_otps (account_id);
