-- Platform schema v2 — per-row telemetry blob + model/cost granularity.
-- Apply with the Supabase CLI (supabase db push / migration up). Mirrors the
-- SQLite _SCHEMA in store.py — keep in sync.

-- 1. Capture/governor seams the proxy ships (model, session_id, brain_ms,
--    tools_*, gov_*, …) land here instead of being dropped. New fields need no
--    migration. JSONB (not text) so we can index + query paths.
ALTER TABLE ledger ADD COLUMN IF NOT EXISTS extra JSONB NOT NULL DEFAULT '{}'::jsonb;

-- Expression index on the model path — the per-model rollup groups/filters on it.
CREATE INDEX IF NOT EXISTS ledger_model ON ledger ((extra ->> 'model'));

-- 2. Model list pricing (USD per million tokens). Cost is computed at report
--    time by joining ledger token sums to this table — never stored per row, so
--    a price change is one UPDATE and never rewrites history. cache_write is the
--    5-minute-TTL rate (1.25x input); the ledger can't distinguish TTL.
--    KEEP IN SYNC with store.py _PRICING_SEED.
CREATE TABLE IF NOT EXISTS model_pricing (
    model                TEXT PRIMARY KEY,
    input_per_mtok       NUMERIC NOT NULL,
    output_per_mtok      NUMERIC NOT NULL,
    cache_read_per_mtok  NUMERIC NOT NULL,
    cache_write_per_mtok NUMERIC NOT NULL,
    currency             TEXT NOT NULL DEFAULT 'USD'
);

INSERT INTO model_pricing
    (model, input_per_mtok, output_per_mtok, cache_read_per_mtok, cache_write_per_mtok)
VALUES
    ('claude-fable-5',   10.0, 50.0, 1.0,  12.5),
    ('claude-opus-4-8',   5.0, 25.0, 0.5,  6.25),
    ('claude-opus-4-7',   5.0, 25.0, 0.5,  6.25),
    ('claude-opus-4-6',   5.0, 25.0, 0.5,  6.25),
    ('claude-sonnet-5',   3.0, 15.0, 0.3,  3.75),
    ('claude-sonnet-4-6', 3.0, 15.0, 0.3,  3.75),
    ('claude-haiku-4-5',  1.0,  5.0, 0.1,  1.25)
ON CONFLICT (model) DO NOTHING;

-- RLS: the service is the only writer (its own role); anon/authenticated get no
-- direct access, same posture as the v1 tables.
ALTER TABLE model_pricing ENABLE ROW LEVEL SECURITY;
