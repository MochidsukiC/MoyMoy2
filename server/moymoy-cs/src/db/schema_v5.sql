-- MoyMoy schema v5 — character ownership is proved per request by a Hub-signed
-- attestation, not looked up in a link table.
--
-- `account_mc_links` was written on charge and then READ as an authorization key
-- (R007: "this character belongs to that account"). Its only evidence was a
-- self-asserted gameUuid from an earlier charge, so whoever asserted a UUID first
-- owned it. Attestation supplies the missing proof per request, which leaves the
-- table as a residue that can only be misused — so it goes.
--
-- What replaces it is NOT a stored proof: `emerald_ops.attester_id` records the
-- server the consent named, purely so a retry is delivered where the original
-- was. Routing information, re-derived from a fresh assertion every time it
-- authorizes anything.

-- 1. Drop the link table (its UNIQUE index goes with it).
DROP TABLE IF EXISTS account_mc_links;

-- 2. Record the consented server on each op. SQLite cannot add a NOT NULL column
--    without a default, so this is nullable; NULL means "written before v5", and
--    step 3 terminates every such op that could still be re-driven.
ALTER TABLE emerald_ops ADD COLUMN attester_id TEXT;

-- 3. Terminate the pre-v5 non-terminal ops. They carry no attester_id, and the
--    auto-route address that used to stand in for one is gone, so reconciliation
--    can never deliver them — leaving them open would mean retrying an unknown
--    destination for 24h and then dead-lettering anyway.
--      pending = nothing was ever delivered ⇒ nothing consumed ⇒ 'failed' is safe.
--      sent    = consumption is UNKNOWN     ⇒ 'stuck' for manual review. Consumed
--                emeralds are never written off silently (the R008 discipline);
--                a late ack can still settle a stuck op.
UPDATE emerald_ops
   SET state = 'failed', updated_unix_ms = CAST(strftime('%s','now') AS INTEGER) * 1000
 WHERE state = 'pending';
UPDATE emerald_ops
   SET state = 'stuck',  updated_unix_ms = CAST(strftime('%s','now') AS INTEGER) * 1000
 WHERE state = 'sent';

-- 4. Move charge idempotency into a per-account namespace. The PK is
--    (idem_key, scope), so widening the scope string needs no table rebuild.
--    Existing rows take the account of the op they created; a row whose op is
--    gone keeps the bare 'charge' scope, which no live code path looks up any
--    more (a fresh charge would simply create a new op rather than replay it).
--    Skipping this would leave the first post-migration retry of an in-flight
--    idem_key missing its replay record and consuming a second time.
UPDATE idempotency
   SET scope = 'charge:' || (SELECT e.account_id FROM emerald_ops e
                              WHERE e.idem_key = idempotency.idem_key LIMIT 1)
 WHERE scope = 'charge'
   AND EXISTS (SELECT 1 FROM emerald_ops e WHERE e.idem_key = idempotency.idem_key);
