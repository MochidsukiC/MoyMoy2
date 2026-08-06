-- MoyMoy schema v9 — merchant revenue moves into escrow.
--
-- Until v8 an approval paid the merchant DIRECTLY: `settle` transferred payer →
-- merchant in the same transaction that claimed the intent, and the money was the
-- shop's the instant the customer pressed approve. DEV.md carried the consequence
-- as an accepted risk — a shop that collected and withdrew its takings to the MC
-- world leaves nothing to reverse, so `force_refund` could only report
-- `MerchantShort` and stop.
--
-- v9 puts a holding account between them. The payer is still debited immediately
-- (nothing about the buyer's experience changes), but the money lands in MoyMoy's
-- own static account and stays there until BOTH of these are true:
--
--   * the merchant has reported the order fulfilled (`fulfilled_unix_ms`), and
--   * the release gate has elapsed (`release_due_unix_ms`).
--
-- Gating on a FULFILMENT REPORT rather than on time alone is the point. A
-- delivery that takes hours keeps its money in escrow for those hours, so there
-- is always something to refund from — which is what retires the accepted risk
-- rather than shortening the window in which it applies.
--
-- The escrow account itself is NOT created here. It is a row in `accounts` with a
-- deterministic id, seeded idempotently at startup by `wallet::seed_escrow_account`
-- — the same treatment the demo merchants' non-login accounts get, and for the
-- same reason: it is data, not structure, and a DB restored from before it
-- existed must grow one on the next boot rather than on the next migration.

-- 1. The escrow lifecycle, on the intent that owns it.
--
--    All nullable, all NULL for every intent that never escrows (created,
--    declined, canceled, expired). `refunded_unix_ms` / `refund_tx_id` from v6 are
--    the shape being followed: a timestamp that also serves as the exactly-once
--    claim, plus the ledger row it produced.
ALTER TABLE payment_intents ADD COLUMN escrowed_unix_ms        INTEGER; -- when the money reached the escrow account
ALTER TABLE payment_intents ADD COLUMN release_due_unix_ms     INTEGER; -- escrowed + the release gate; the sweep will not act before this
ALTER TABLE payment_intents ADD COLUMN escrow_deadline_unix_ms INTEGER; -- escrowed + the no-report deadline (the branch that acts on it is NOT in v9)
ALTER TABLE payment_intents ADD COLUMN fulfilled_unix_ms       INTEGER; -- when the merchant reported the order fulfilled; also the once-only claim
ALTER TABLE payment_intents ADD COLUMN fulfilled_amount        INTEGER; -- what the merchant is owed, in MINOR UNITS (1/100 エメ, as of v8); 0 <= x <= amount
ALTER TABLE payment_intents ADD COLUMN released_unix_ms        INTEGER; -- when escrow paid out; the sweep's exactly-once claim
ALTER TABLE payment_intents ADD COLUMN release_tx_id           TEXT;    -- escrow → merchant
ALTER TABLE payment_intents ADD COLUMN escrow_refund_tx_id     TEXT;    -- escrow → payer, for `amount - fulfilled_amount`

-- 2. The release sweep's index.
--
--    Partial on `released_unix_ms IS NULL`, so it holds only what is still owed:
--    the sweep runs every 30 seconds for the life of the process and must not walk
--    the whole payment history to find the handful of rows that are due. Rows drop
--    out of the index the moment they are released, which is also why the two
--    columns the sweep filters on are the ones indexed.
CREATE INDEX IF NOT EXISTS idx_intents_release_due
    ON payment_intents (release_due_unix_ms)
    WHERE released_unix_ms IS NULL AND escrowed_unix_ms IS NOT NULL;

-- 3. Close the books on every payment that predates escrow.
--
--    **This is the load-bearing statement in v9.** Every `paid` intent that exists
--    right now was settled the old way, so its money is ALREADY in the merchant's
--    account. Left with a NULL `released_unix_ms` those rows match the sweep's
--    filter, and the first sweep after deployment would pay each of them a second
--    time — out of the escrow account, which for these rows never held anything.
--
--    Stamping them released (at the migration instant, with no tx ids, because no
--    ledger row was written here) is what states the truth: these were settled,
--    and not by this mechanism. `escrowed_unix_ms` stays NULL, which is what
--    distinguishes them from anything v9 handles and keeps them out of the partial
--    index above.
--
--    The state filter keeps this off the intents that were never paid at all. The
--    `IS NULL` guard is not for the migration — `user_version` already runs this
--    file exactly once — but for a hand re-run during incident recovery, where
--    re-stamping an already-closed row with a later timestamp would misreport when
--    its money moved.
UPDATE payment_intents
   SET released_unix_ms = CAST(strftime('%s','now') AS INTEGER) * 1000
 WHERE state = 'paid' AND released_unix_ms IS NULL;
