-- MoyMoy cs.mnn wallet — SQLite schema (design-derived from "MochiOS Mobile.html").
--
-- Currency is integer エメ (eme); 9 eme = 1 emerald block (Minecraft). The wallet
-- is the single source of truth for balances; the in-world mod is the source of
-- truth for emerald CONSUMPTION (reconciled via emerald_ops).
--
-- Applied by db::migrate() which steps PRAGMA user_version. This file is the v1
-- baseline (idempotent CREATE IF NOT EXISTS).

-- Connection pragmas are set per-connection in db::mod.rs (WAL, foreign_keys,
-- busy_timeout); they are not persisted here.

-- One wallet account per Minecraft player (keyed by canonical MC UUID).
CREATE TABLE IF NOT EXISTS accounts (
    account_id      TEXT PRIMARY KEY,                 -- canonical mc_uuid (lowercase, hyphenated)
    mcid            TEXT,                             -- latest known display name
    balance         INTEGER NOT NULL DEFAULT 0 CHECK (balance >= 0),  -- エメ
    -- cosmetic card face (design: holder / number / expiry)
    holder          TEXT NOT NULL DEFAULT 'PLAYER',
    card_number     TEXT NOT NULL DEFAULT '',         -- stable pseudo card number derived from uuid
    card_expiry     TEXT NOT NULL DEFAULT '07/29',
    is_merchant     INTEGER NOT NULL DEFAULT 0,       -- 1 ⇒ this account is a shop (pay target)
    created_unix_ms INTEGER NOT NULL,
    updated_unix_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_accounts_mcid ON accounts (mcid);

-- Append-only ledger. amount is SIGNED from THIS account's perspective
-- (positive = credit/received, negative = debit/spent). kind ∈
-- pay | send | receive | charge.
CREATE TABLE IF NOT EXISTS transactions (
    id                TEXT PRIMARY KEY,               -- UUID v4
    account_id        TEXT NOT NULL REFERENCES accounts (account_id),
    kind              TEXT NOT NULL,                  -- pay|send|receive|charge
    label             TEXT NOT NULL,                  -- display label (counterparty/merchant/"インベントリのエメラルド")
    counterparty_id   TEXT,                           -- the other account_id (null for charge)
    counterparty_name TEXT,
    amount            INTEGER NOT NULL,               -- signed
    balance_after     INTEGER NOT NULL,
    memo              TEXT,
    ts_unix_ms        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tx_account_time ON transactions (account_id, ts_unix_ms DESC);

-- Registered shops (the "pay" tab targets). Each merchant has a backing account
-- that receives the payment. `dist` (proximity) is a presence/MC concern and is
-- not stored here.
CREATE TABLE IF NOT EXISTS merchants (
    merchant_id     TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts (account_id),
    name            TEXT NOT NULL,
    sub             TEXT,                             -- subtitle ("総合ストア" 等)
    glyph           TEXT,                             -- design glyph
    pal             TEXT,                             -- design palette key
    created_unix_ms INTEGER NOT NULL
);

-- HTTP-layer idempotency: a client-supplied idem_key maps to the frozen JSON
-- response so a retried POST replays the exact prior outcome (no double-spend).
CREATE TABLE IF NOT EXISTS idempotency (
    idem_key        TEXT PRIMARY KEY,
    scope           TEXT NOT NULL,                    -- "send" | "pay" | "charge"
    response_json   TEXT NOT NULL,
    created_unix_ms INTEGER NOT NULL
);

-- Emerald-charge ledger: the bridge between the mod (truth of consumption) and
-- the wallet (truth of balance). op_id is the command-bus idempotency key. The
-- balance is credited ONLY on the mod's settlement ack (state → settled),
-- never on send — see charge.rs.
CREATE TABLE IF NOT EXISTS emerald_ops (
    op_id            TEXT PRIMARY KEY,                -- backend-generated (command-bus idempotency)
    idem_key         TEXT NOT NULL,                   -- HTTP-layer key that started it
    account_id       TEXT NOT NULL REFERENCES accounts (account_id),
    mc_uuid          TEXT NOT NULL,
    direction        TEXT NOT NULL DEFAULT 'charge',  -- charge (emerald→eme)
    requested_amount INTEGER NOT NULL,
    settled_amount   INTEGER,                         -- consumed reported by mod
    state            TEXT NOT NULL,                   -- pending|sent|settled|failed
    created_unix_ms  INTEGER NOT NULL,
    updated_unix_ms  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ops_state ON emerald_ops (state);
