-- MoyMoy schema v8 — amounts become minor units (1/100 エメ).
--
-- Every stored amount was an integer count of エメ, so the wallet could not hold
-- (or bill) anything smaller than one whole エメ. From here on every amount
-- column is an integer count of HUNDREDTHS of an エメ: a balance of 1550 is
-- 15.50 エメ. Nothing about what an account is worth changes — this migration is
-- a pure ×100 on every amount column, applied in the one transaction that also
-- stamps user_version, so a DB is never half-converted.
--
-- The conversion between minor units and physical emeralds (which are indivisible
-- and cannot be scaled) lives entirely in the mod-facing adapter, src/mc.rs —
-- see the module docs there. This file only rescales what is already stored.
--
-- COLUMNS THAT MUST NOT BE SCALED, and why they are named here rather than left
-- to be inferred: merchants.max_open_intents (a count of intents),
-- accounts.failed_pin_attempts / moymoy_otps.attempts / notification_outbox.attempts
-- (counts of tries), and every *_unix_ms (a timestamp). Anything added later that
-- is not an amount belongs on this list, not below.

-- 1. Balances and the ledger that has to keep agreeing with them.
--    accounts.balance == SUM(transactions.amount) per account is an invariant of
--    this wallet, so both sides scale by the same factor in the same statement
--    batch or the check breaks.
UPDATE accounts      SET balance = balance * 100;
UPDATE transactions  SET amount = amount * 100, balance_after = balance_after * 100;

-- 2. Amounts a customer is asked for, and amounts a device is told about.
UPDATE payment_intents     SET amount = amount * 100;
UPDATE notification_outbox SET amount = amount * 100;

-- 3. Merchant issuance ceiling. NULL means "the code's default applies", and
--    NULL * 100 is NULL, so an unset cap stays unset (the default itself is
--    scaled in merchant.rs). max_open_intents is a COUNT and is left alone.
UPDATE merchants SET daily_issue_cap = daily_issue_cap * 100 WHERE daily_issue_cap IS NOT NULL;

-- 4. The emerald op ledger. BOTH amount columns are scaled, and this is the one
--    decision in v8 that is worth spelling out, because "settled_amount is what
--    the mod consumed" would argue for leaving it as a physical emerald count.
--
--    It is not that. charge.rs writes `credited` — the amount CREDITED TO THE
--    BALANCE, already clamped to requested_amount — into settled_amount, and
--    withdraw.rs writes the granted amount it compares against requested_amount
--    in the same expression. Both columns are therefore already ledger amounts
--    that sit next to accounts.balance, and requested_amount is fed straight back
--    to the mod adapter by the reconciliation re-send.
--
--    Leaving these two as physical counts would create exactly the defect this
--    migration exists to remove: two columns in the same table, compared against
--    each other by the settlers, silently counting in different units. So they
--    scale with everything else, and the ÷100 needed to talk to the mod happens
--    in src/mc.rs where it is visible.
UPDATE emerald_ops SET requested_amount = requested_amount * 100;
UPDATE emerald_ops SET settled_amount = settled_amount * 100 WHERE settled_amount IS NOT NULL;

-- 5. Idempotency: amounts frozen inside response_json.
--
--    These rows are NOT deleted. A deleted record makes the client's next retry
--    re-execute the operation, which for a send or a payment means paying twice —
--    so the values are rewritten in place and the keys keep their frozen replies.
--
--    Three shapes exist, and only these three carry an amount:
--      scope 'send' / 'pay'  → $.balance   (the payer's balance after the transfer)
--      scope 'mi:<merchant>' → $.amount    (a merchant intent's amount)
--      scope 'charge:<acct>' / 'withdraw:<acct>' → {ok, op_id, state}, no amount
--    The charge/withdraw records are deliberately untouched: there is nothing in
--    them to rescale, and rewriting them would only risk corrupting an op_id.
UPDATE idempotency
   SET response_json = json_set(response_json, '$.balance',
                                json_extract(response_json, '$.balance') * 100)
 WHERE scope IN ('send', 'pay')
   AND json_extract(response_json, '$.balance') IS NOT NULL;

--    The merchant-facing intent reply also RENAMES its key. /merchant/v1 now
--    speaks `amount_minor` and refuses a request carrying the old `amount`
--    (merchant.rs), so a replay still answering `amount` would be the one path
--    that hands an integrator a number in a unit the live API no longer accepts —
--    and an integrator that reads `amount` would take the scaled value as whole
--    エメ. The rename and the scale therefore happen together.
UPDATE idempotency
   SET response_json = json_remove(
           json_set(response_json, '$.amount_minor',
                    json_extract(response_json, '$.amount') * 100),
           '$.amount')
 WHERE scope LIKE 'mi:%'
   AND json_extract(response_json, '$.amount') IS NOT NULL;
