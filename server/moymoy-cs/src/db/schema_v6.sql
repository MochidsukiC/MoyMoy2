-- MoyMoy schema v6 — merchants become self-serve and credentialled, and a
-- PaymentIntent becomes the only way a shop can ask a wallet for money.
--
-- Until v5 a "merchant" was a seeded display row with no owner, no credential and
-- no lifecycle: `/wallet/pay` sent an arbitrary amount to whichever merchant_id
-- the client named. v6 gives a merchant the three things that were missing — a
-- login account that can actually withdraw the takings (`owner_account_id`), a
-- credential that proves an API caller is that shop (`api_key_hash`), and a
-- status that can stop it (`status`) — and moves the amount out of the client's
-- reach into `payment_intents`.
--
-- `listed` defaults to 0 (see step 2): registration is self-serve now, so a
-- merchant appearing in every user's pay tab the moment it registers would make
-- the list an advertising surface for anyone.

-- 1. Extend merchants into an owned, credentialled, stoppable shop.
--    ADD COLUMN with a REFERENCES clause is accepted because the default is NULL.
ALTER TABLE merchants ADD COLUMN owner_account_id          TEXT REFERENCES accounts (account_id);
ALTER TABLE merchants ADD COLUMN name_skeleton             TEXT;     -- UTS #39 confusable skeleton; uniqueness is decided on this, not on `name`
ALTER TABLE merchants ADD COLUMN status                    TEXT NOT NULL DEFAULT 'active';  -- active|disabled
ALTER TABLE merchants ADD COLUMN api_key_hash              TEXT;     -- SHA-256(key), base64; the key itself is never stored
ALTER TABLE merchants ADD COLUMN api_key_prefix            TEXT;     -- leading chars, for "which key is this" in the portal
ALTER TABLE merchants ADD COLUMN api_key_created_unix_ms   INTEGER;
ALTER TABLE merchants ADD COLUMN api_key_last_used_unix_ms INTEGER;  -- leak detection: a key used while its owner is idle
ALTER TABLE merchants ADD COLUMN listed                    INTEGER NOT NULL DEFAULT 0;
ALTER TABLE merchants ADD COLUMN payer_ref_salt            TEXT;     -- 128-bit, per merchant; derives payer_ref so refs don't correlate across shops
ALTER TABLE merchants ADD COLUMN max_open_intents          INTEGER;  -- NULL ⇒ the new-merchant default in merchant.rs
ALTER TABLE merchants ADD COLUMN daily_issue_cap           INTEGER;  -- NULL ⇒ the new-merchant default in merchant.rs
ALTER TABLE merchants ADD COLUMN updated_unix_ms           INTEGER;

-- 2. Demote the seeded demo merchants (m1..m5). Their backing accounts have a
--    NULL handle and a NULL pin_hash, so nobody can log into them and nobody can
--    withdraw what they take in: money paid to one is money destroyed. They stay
--    as rows (the ledger references them) but they leave the pay tab.
UPDATE merchants SET listed = 0, updated_unix_ms = created_unix_ms;

-- 3. Uniqueness on what a name LOOKS like. Both indexes are partial: a merchant
--    whose skeleton collided at migration time keeps a NULL (see
--    db::backfill_merchant_skeletons) and a merchant with no key yet keeps a NULL
--    api_key_hash, and neither may block the other rows from being unique.
CREATE UNIQUE INDEX IF NOT EXISTS idx_merchants_skeleton
    ON merchants (name_skeleton) WHERE name_skeleton IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_merchants_api_key
    ON merchants (api_key_hash) WHERE api_key_hash IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_merchants_owner ON merchants (owner_account_id);

-- 4. PaymentIntents: a merchant's request for a specific amount, which only the
--    payer's own session can turn into a transfer.
--
--    No CHECK on `state` — the existing emerald_ops does not have one either, and
--    the vocabulary is closed in code (payments::state) where a typo is a compile
--    error rather than a runtime constraint violation on a live wallet.
--
--    `amount` and `merchant_id` are the authority for what gets paid to whom:
--    approve reads them from HERE, never from the approving request, which is
--    what makes the amount untamperable by the client that carries the intent_id.
CREATE TABLE IF NOT EXISTS payment_intents (
    intent_id             TEXT PRIMARY KEY,                            -- 'pi_' + 128-bit CSPRNG
    merchant_id           TEXT NOT NULL REFERENCES merchants (merchant_id),
    amount                INTEGER NOT NULL,                            -- エメ
    description           TEXT NOT NULL,                               -- merchant-supplied, sanitized at creation
    order_ref             TEXT,                                        -- the merchant's own order id (opaque here)
    state                 TEXT NOT NULL,                               -- created|paid|declined|canceled|expired
    payer_account_id      TEXT REFERENCES accounts (account_id),       -- who actually paid / declined
    payer_hint_account_id TEXT,                                        -- who the merchant expects; a mismatch is refused
    launch_app_id         TEXT,                                        -- self-declared by the merchant; the approval screen warns on mismatch
    tx_id                 TEXT,                                        -- the payer's ledger row, once paid
    -- An operator-forced refund. `paid` is terminal and stays terminal: a refund
    -- is a second, opposite movement, not a rewind. These two are what make it
    -- happen exactly once — the claim is `refunded_unix_ms IS NULL`, in the same
    -- statement-guard discipline as charge/withdraw settlement.
    refunded_unix_ms      INTEGER,
    refund_tx_id          TEXT,
    idem_key              TEXT NOT NULL,
    created_unix_ms       INTEGER NOT NULL,
    updated_unix_ms       INTEGER NOT NULL,
    expires_unix_ms       INTEGER NOT NULL
);
-- The merchant's own listing / issuance-cap window.
CREATE INDEX IF NOT EXISTS idx_intents_merchant_time
    ON payment_intents (merchant_id, created_unix_ms DESC);
-- The expiry sweep: one index scan over exactly the rows that can still expire.
CREATE INDEX IF NOT EXISTS idx_intents_state_expiry
    ON payment_intents (state, expires_unix_ms);
