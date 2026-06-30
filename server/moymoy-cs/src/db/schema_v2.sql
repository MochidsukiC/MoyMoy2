-- MoyMoy schema v2 — independent MoyMoy accounts (handle + PIN), backend-verified
-- sessions, and per-account MC-character links.
--
-- Identity redesign: account_id stops meaning "the Minecraft UUID" and becomes a
-- server-generated MoyMoy account id (PayPay-style). A person logs in with a
-- handle + PIN; the wallet is keyed by their MoyMoy account. The Minecraft UUID
-- is demoted to a *linked resource* used only for emerald charging (recorded in
-- account_mc_links, routed by emerald_ops.mc_uuid).
--
-- Applied by db::migrate() as the `version < 2` step (additive ALTERs + new
-- tables). Legacy mc_uuid-keyed wallet rows are reset (balances are disposable
-- dev data — agreed in the plan); accounts are re-created via /auth/register and
-- demo merchants re-seed at startup.

-- 1. Extend the existing accounts table into a MoyMoy account.
--    (ADD COLUMN keeps existing FKs from transactions/merchants/emerald_ops.)
ALTER TABLE accounts ADD COLUMN handle               TEXT;     -- login id / @send target (as entered)
ALTER TABLE accounts ADD COLUMN handle_lower         TEXT;     -- case-insensitive uniqueness + lookup key
ALTER TABLE accounts ADD COLUMN display_name         TEXT;     -- friendly name (card holder / counterparty label)
ALTER TABLE accounts ADD COLUMN pin_hash             TEXT;     -- Argon2id PHC string; NULL ⇒ non-login (merchant/system)
ALTER TABLE accounts ADD COLUMN failed_pin_attempts  INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN locked_until_unix_ms INTEGER;  -- lockout window after repeated PIN failures

-- Handles are unique case-insensitively; the partial index lets merchant/system
-- accounts (NULL handle) coexist without colliding.
CREATE UNIQUE INDEX IF NOT EXISTS idx_accounts_handle_lower
    ON accounts (handle_lower) WHERE handle_lower IS NOT NULL;

-- 2. Sessions: a login mints a random token; we persist only its SHA-256 hash
--    (never the token itself), with an expiry. Resolving a request hashes the
--    presented X-MoyMoy-Session and matches a non-expired row.
CREATE TABLE IF NOT EXISTS moymoy_sessions (
    session_id        TEXT PRIMARY KEY,                 -- UUID v4
    account_id        TEXT NOT NULL REFERENCES accounts (account_id),
    token_hash        TEXT NOT NULL UNIQUE,             -- SHA-256(token), base64
    phone_id          TEXT,                             -- device metadata (self-asserted; not a security boundary)
    created_unix_ms   INTEGER NOT NULL,
    last_seen_unix_ms INTEGER NOT NULL,
    expires_unix_ms   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_account ON moymoy_sessions (account_id);

-- 3. Account ↔ Minecraft-character links (1 account : many MC chars). Auto-filled
--    on charge (the gameUuid is verified by the in-world consume). Charge routes
--    by mc_uuid; the credited balance goes to account_id.
CREATE TABLE IF NOT EXISTS account_mc_links (
    account_id     TEXT NOT NULL REFERENCES accounts (account_id),
    mc_uuid        TEXT NOT NULL,
    mcid           TEXT,
    linked_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, mc_uuid)
);
CREATE INDEX IF NOT EXISTS idx_mc_links_uuid ON account_mc_links (mc_uuid);

-- 4. Drop the legacy accounts.mcid index — mcid now lives in account_mc_links and
--    accounts.mcid is never read post-v2. Keeping the index would make every
--    accounts write maintain a stale, unused B-tree entry.
DROP INDEX IF EXISTS idx_accounts_mcid;

-- 5. Reset legacy mc_uuid-keyed wallet data (children before parents for FK).
--    Demo merchants re-seed at startup; user accounts re-register.
DELETE FROM transactions;
DELETE FROM emerald_ops;
DELETE FROM idempotency;
DELETE FROM merchants;
DELETE FROM accounts;
