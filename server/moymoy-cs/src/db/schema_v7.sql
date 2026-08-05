-- MoyMoy schema v7 — deposit notifications.
--
-- 1. WHERE to notify: each session carries the Mochi account (device owner) the
--    app registered via POST /wallet/link. Self-asserted, the same trust model
--    as mail's /mail/link (APNs-style device registration): it directs only
--    where THIS account's own deposit notifications go, and moves no money.
--    Living on the session row (not the account) is deliberate — one device
--    holds several accounts and one account logs in from several devices, so an
--    account-level value would let the last-linked device take every
--    notification. Logout deletes the session row and resolve filters expiry,
--    so a link dies with its session and needs no unlink API.
ALTER TABLE moymoy_sessions ADD COLUMN mochi_account_id TEXT;

-- 2. WHAT to notify: a transactional outbox written by wallet.rs inside the
--    SAME transaction as the balance credit — a row exists iff its credit
--    committed, so a rolled-back operation can never notify. Drained
--    best-effort by src/notify.rs (delete on delivery, bounded retry), so rows
--    are transient; the ledger, not this table, is the record of the deposit.
CREATE TABLE IF NOT EXISTS notification_outbox (
    outbox_id            TEXT PRIMARY KEY,
    account_id           TEXT NOT NULL REFERENCES accounts (account_id),
    kind                 TEXT NOT NULL,     -- the credited txn's kind (receive|charge|withdraw)
    label                TEXT NOT NULL,     -- the credited txn's label, verbatim
    amount               INTEGER NOT NULL,  -- positive エメ
    created_unix_ms      INTEGER NOT NULL,
    attempts             INTEGER NOT NULL DEFAULT 0,
    next_attempt_unix_ms INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_outbox_due ON notification_outbox (next_attempt_unix_ms);
