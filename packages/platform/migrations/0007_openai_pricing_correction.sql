-- Platform schema v7 — correct the OpenAI rows seeded by 0004.
--
-- Verified against OpenAI's own pricing page (developers.openai.com/api/docs/
-- pricing) on 2026-08-24. Two defects in the 0004 seed:
--
-- 1. gpt-5.6-sol was seeded at $5/$30 with cache_read $0.50. The published
--    short-context rate is $4.00 input / $0.40 cached / $20.00 output — the
--    0004 numbers predate a cut. sol is the model real Codex ChatGPT-
--    subscription traffic bills against, so every Codex row has been priced
--    25% high on input and 50% high on output.
-- 2. Every OpenAI row carries cache_write = 1.25x input, copied from
--    Anthropic's 5-minute-TTL surcharge. OpenAI has NO cache-write charge:
--    its pricing table has three columns (input / cached input / output) and
--    a freshly-cached token bills at the plain input rate. Inert today —
--    openai.rs::ledger_usage always writes cache_creation_input_tokens = 0
--    because the Responses usage block reports no write bucket — but a wrong
--    rate sitting in the table will misprice the moment that changes.
--
-- Unchanged because they check out: gpt-5.6-terra ($2/$0.20/$12),
-- gpt-5.6-luna ($0.20/$0.02/$1.20), gpt-5.3-codex ($1.75/$0.175/$14),
-- gpt-5-mini, gpt-5-nano. gpt-5.5 and the gpt-5.4-* rows are no longer
-- listed (retired); their rates are left as seeded.
--
-- Still the short-context tier only: gpt-5.6-sol long-context bills
-- $8/$0.80/$30 above the threshold, and a single flat row cannot express a
-- tier break, so long-context spend stays undervalued — never overvalued.
-- KEEP IN SYNC with store.py _PRICING_SEED.

UPDATE model_pricing
   SET input_per_mtok = 4.0,
       output_per_mtok = 20.0,
       cache_read_per_mtok = 0.4
 WHERE model = 'gpt-5.6-sol';

-- No cache-write surcharge on this provider: a written cache entry bills at
-- the plain input rate.
UPDATE model_pricing
   SET cache_write_per_mtok = input_per_mtok
 WHERE model LIKE 'gpt-%';

-- Models live on the pricing page with no row yet.
INSERT INTO model_pricing
    (model, input_per_mtok, output_per_mtok, cache_read_per_mtok, cache_write_per_mtok)
VALUES
    ('gpt-5.6-cyber', 12.5,  75.0,  1.25,  12.5),
    ('gpt-5',          1.25, 10.0,  0.125,  1.25),
    -- gpt-5-pro publishes NO cached-input rate: prompt caching is not offered
    -- on it, so cache_read is the full input rate rather than a fabricated
    -- discount.
    ('gpt-5-pro',     15.0, 120.0, 15.0,   15.0)
ON CONFLICT (model) DO NOTHING;
