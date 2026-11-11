-- MoyMoy schema v13 — QR payments (dynamic and static).
--
-- A shop can now be paid by a QR code instead of only by an API-created intent:
--
--   * dynamic QR: the shop's backend creates an intent as before and prints its
--     id as a QR; the QR opens a landing page that launches the MoyMoy app.
--   * static QR:  the shop prints ONE code that never changes, and the CUSTOMER
--     types the amount. The wallet then creates the intent itself, on the
--     customer's behalf, addressed to that customer only.
--
-- Nothing in the approval / escrow path changes. A static-QR intent is an
-- ordinary `payment_intents` row that goes through the same PIN, the same
-- step-up and the same escrow as any other.

-- 1. The shop's static QR code: 'sq_' + 128-bit CSPRNG, base64url.
--
--    NULL until the owner issues one. Issuing again OVERWRITES it (the old code
--    stops resolving), which is how a printed code that has leaked or been
--    defaced is retired. Closing the shop sets it back to NULL.
--
--    Not a secret — it is printed on a public block — but unguessable, so a code
--    cannot be enumerated into a list of shops. The index is partial for the same
--    reason `idx_merchants_api_key` is: rows without a code must not collide.
ALTER TABLE merchants ADD COLUMN static_qr_code TEXT;
CREATE UNIQUE INDEX idx_merchants_static_qr
    ON merchants (static_qr_code) WHERE static_qr_code IS NOT NULL;

-- 2. Who created an intent: 'api' (the shop, with its API key) or 'static_qr'
--    (a customer, by scanning the shop's static code).
--
--    The shop's issuance ceilings count only 'api' rows. A customer-created bill
--    must not count against the shop's open-intent or daily-issue ceiling, or
--    anybody holding the printed code could stop the shop from billing simply by
--    creating bills. Every pre-v13 row was created by a shop, hence the default.
ALTER TABLE payment_intents ADD COLUMN source TEXT NOT NULL DEFAULT 'api';
