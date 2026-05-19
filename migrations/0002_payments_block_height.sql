-- Add `block_height` to payments so confirmation depth can be computed
-- from chain reality (chain_tip - block_height + 1) instead of by
-- counting poll ticks.
--
-- The pre-migration matcher incremented `confirmations` by +1 on every
-- sync iteration regardless of whether the chain actually advanced.
-- With poll_interval_secs = 30 and PIVX's ~60s block time, a payment
-- could reach "3 confirmations" in 90s of wall-clock time even though
-- only 1 actual block had been mined.
--
-- Existing payment rows get a `block_height = 0` default; the matcher
-- treats 0 as "still mempool" and will populate the real height on the
-- next sync that re-observes the UTXO. No data loss, just a single
-- sync-tick delay for legacy rows.

ALTER TABLE payments ADD COLUMN block_height INTEGER NOT NULL DEFAULT 0;
