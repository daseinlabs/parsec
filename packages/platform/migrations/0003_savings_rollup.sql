-- Platform schema v3 — incremental (account, model) rollup for savings reads.
-- Apply with the Supabase CLI (supabase db push / migration up). Mirrors the
-- SQLite _SCHEMA in store.py — keep in sync.
--
-- Why: /savings/public used to GROUP BY over the whole ledger on every cache
-- miss (~7.5s at 562k rows, and O(ledger) forever). The rollup keeps one row
-- per (account, model) with running totals, maintained by a ledger trigger,
-- so reads are a users×models-row scan instead. Measurement honesty (§8.4) is
-- preserved: every counter is derived from the same per-request count_tokens
-- rows and stays recomputable — to verify or rebuild, re-run the backfill
-- INSERT below after TRUNCATE savings_rollup (under the same ledger lock).
--
-- The trigger (not app code) maintains the rollup so the expand/contract
-- deploy window is safe: code still running the old savings_public keeps the
-- rollup exact from the moment this migration commits.

-- Backfill and trigger installation must see the same ledger: block writers
-- (reads still fine) so no row lands between the backfill snapshot and the
-- trigger going live. Held only for this transaction (~seconds).
LOCK TABLE ledger IN EXCLUSIVE MODE;

CREATE TABLE IF NOT EXISTS savings_rollup (
    account_id                  TEXT NOT NULL,
    -- '' = the row carried no model (extra->>'model' IS NULL); a real NULL
    -- can't be part of a primary key. Readers NULLIF it back.
    model                       TEXT NOT NULL,
    rows_count                  BIGINT NOT NULL DEFAULT 0,
    measured_rows               BIGINT NOT NULL DEFAULT 0,
    tokens_saved                BIGINT NOT NULL DEFAULT 0,
    counterfactual_input_tokens BIGINT NOT NULL DEFAULT 0,
    billed_input_tokens         BIGINT NOT NULL DEFAULT 0,
    billed_output_tokens        BIGINT NOT NULL DEFAULT 0,
    billed_cache_read_tokens    BIGINT NOT NULL DEFAULT 0,
    billed_cache_write_tokens   BIGINT NOT NULL DEFAULT 0,
    fail_open_count             BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, model)
);

-- Fold one ledger row into the rollup with the given sign (+1 add, -1 remove).
-- Same §8.4 arithmetic as the ledger scans: a NULL counterfactual is a hole —
-- it counts in rows_count but adds nothing to measured_rows/tokens_saved.
CREATE OR REPLACE FUNCTION savings_rollup_step(r ledger, sign BIGINT)
RETURNS void AS $$
    INSERT INTO savings_rollup (account_id, model, rows_count, measured_rows,
        tokens_saved, counterfactual_input_tokens,
        billed_input_tokens, billed_output_tokens,
        billed_cache_read_tokens, billed_cache_write_tokens, fail_open_count)
    VALUES (
        r.account_id,
        COALESCE(r.extra->>'model', ''),
        sign,
        sign * (r.counterfactual_input_tokens IS NOT NULL)::int,
        sign * COALESCE(r.counterfactual_input_tokens - (r.billed_input_tokens
            + r.billed_cache_read_tokens + r.billed_cache_write_tokens), 0),
        sign * COALESCE(r.counterfactual_input_tokens, 0),
        sign * r.billed_input_tokens,
        sign * r.billed_output_tokens,
        sign * r.billed_cache_read_tokens,
        sign * r.billed_cache_write_tokens,
        sign * r.fail_open::int
    )
    ON CONFLICT (account_id, model) DO UPDATE SET
        rows_count                  = savings_rollup.rows_count                  + EXCLUDED.rows_count,
        measured_rows               = savings_rollup.measured_rows               + EXCLUDED.measured_rows,
        tokens_saved                = savings_rollup.tokens_saved                + EXCLUDED.tokens_saved,
        counterfactual_input_tokens = savings_rollup.counterfactual_input_tokens + EXCLUDED.counterfactual_input_tokens,
        billed_input_tokens         = savings_rollup.billed_input_tokens         + EXCLUDED.billed_input_tokens,
        billed_output_tokens        = savings_rollup.billed_output_tokens        + EXCLUDED.billed_output_tokens,
        billed_cache_read_tokens    = savings_rollup.billed_cache_read_tokens    + EXCLUDED.billed_cache_read_tokens,
        billed_cache_write_tokens   = savings_rollup.billed_cache_write_tokens   + EXCLUDED.billed_cache_write_tokens,
        fail_open_count             = savings_rollup.fail_open_count             + EXCLUDED.fail_open_count;
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION savings_rollup_apply() RETURNS trigger AS $$
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM savings_rollup_step(OLD, -1);
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM savings_rollup_step(NEW, 1);
    END IF;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS ledger_savings_rollup ON ledger;
CREATE TRIGGER ledger_savings_rollup
    AFTER INSERT OR UPDATE OR DELETE ON ledger
    FOR EACH ROW EXECUTE FUNCTION savings_rollup_apply();

-- Backfill from the ledger as it stands (TRUNCATE makes a re-run idempotent).
TRUNCATE savings_rollup;
INSERT INTO savings_rollup (account_id, model, rows_count, measured_rows,
    tokens_saved, counterfactual_input_tokens,
    billed_input_tokens, billed_output_tokens,
    billed_cache_read_tokens, billed_cache_write_tokens, fail_open_count)
SELECT account_id,
    COALESCE(extra->>'model', ''),
    COUNT(*),
    COUNT(counterfactual_input_tokens),
    COALESCE(SUM(counterfactual_input_tokens - (billed_input_tokens
        + billed_cache_read_tokens + billed_cache_write_tokens))
        FILTER (WHERE counterfactual_input_tokens IS NOT NULL), 0),
    COALESCE(SUM(counterfactual_input_tokens), 0),
    COALESCE(SUM(billed_input_tokens), 0),
    COALESCE(SUM(billed_output_tokens), 0),
    COALESCE(SUM(billed_cache_read_tokens), 0),
    COALESCE(SUM(billed_cache_write_tokens), 0),
    COALESCE(SUM(fail_open::int), 0)
FROM ledger
GROUP BY account_id, COALESCE(extra->>'model', '');

-- Same posture as the v1/v2 tables: the service role is the only reader/writer.
ALTER TABLE savings_rollup ENABLE ROW LEVEL SECURITY;
