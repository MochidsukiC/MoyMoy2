-- MoyMoy schema v12 — an unreported payment is parked, not refunded.
--
-- v9 gave escrow a no-report deadline and 3-1b made it return the whole payment
-- to the buyer. That was wrong, and this corrects it before it ever ran in
-- production.
--
-- **Silence is not evidence.** A shop that has not reported has not said the
-- goods stayed put; it has said nothing. Refunding on a timer decides the
-- question in the buyer's favour on no evidence, and the shop-side path that
-- makes this concrete is real: PiggleShop defers a delivery indefinitely while
-- the infrastructure that resolves a player is down (`orders::delivery_deferred`
-- never touches `delivery_attempts`, and a test is named for that property), and
-- it only reports fulfilment once an order reaches `delivered`. So an outage
-- longer than the deadline produced: refund at six hours, successful delivery at
-- seven, and a buyer holding both the goods and the money.
--
-- This repository already answered the same question the other way. When the
-- emerald mod cannot say whether a payout was granted, `emerald_ops` goes to
-- `stuck` and is NEVER auto-refunded, precisely because refunding an unprovable
-- payout hands the player the emeralds and the eme (charge.rs, R008). "We do not
-- know" is its own outcome there, and it is its own outcome here.
--
-- So the deadline now parks: the money stays in escrow, this column records that
-- a human needs to look, and nothing is decided automatically. `released_unix_ms`
-- is deliberately NOT written — a parked intent is unresolved, and marking it
-- released would close it to the operator paths (`force_refund`) that are the
-- whole point of keeping it open.
ALTER TABLE payment_intents ADD COLUMN escrow_parked_unix_ms INTEGER;

-- Keep the release sweep's working set bounded.
--
-- The partial index held "escrowed and not yet released", which used to empty
-- itself: every row left within six hours, by one end or the other. Parking
-- breaks that — a parked row stays escrowed and unreleased until a person acts,
-- so without this it would sit in the index being re-scanned every 30 seconds
-- for as long as it takes somebody to notice.
--
-- Excluding parked rows restores the bound, and costs nothing: the sweep has no
-- interest in them either (it must not re-park a row, or the warning it emits
-- would repeat every tick until the log said nothing). Rebuilt rather than added
-- to, because a partial index's predicate cannot be altered in place.
DROP INDEX IF EXISTS idx_intents_release_due;
CREATE INDEX IF NOT EXISTS idx_intents_release_due
    ON payment_intents (release_due_unix_ms)
    WHERE released_unix_ms IS NULL
      AND escrowed_unix_ms IS NOT NULL
      AND escrow_parked_unix_ms IS NULL;
