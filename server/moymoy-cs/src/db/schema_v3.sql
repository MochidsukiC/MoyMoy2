-- MoyMoy schema v3 — a Minecraft character belongs to exactly ONE MoyMoy account
-- (R007). A character's emeralds fund a single wallet, so a charge / inventory
-- query for a character already claimed by another account is rejected. This is
-- enforced by a UNIQUE index on account_mc_links(mc_uuid), replacing the v2
-- non-unique index.

-- Drop any accidental duplicates (same mc_uuid linked under multiple accounts,
-- which the v2 non-unique index allowed) before adding the unique constraint —
-- keep the earliest link.
DELETE FROM account_mc_links WHERE rowid NOT IN (
    SELECT MIN(rowid) FROM account_mc_links GROUP BY mc_uuid
);

DROP INDEX IF EXISTS idx_mc_links_uuid;
CREATE UNIQUE INDEX IF NOT EXISTS idx_mc_links_uuid ON account_mc_links (mc_uuid);
