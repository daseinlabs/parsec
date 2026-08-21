-- Platform schema v5 — Google Gemini model pricing.
-- Same §8.4 hole as 0004: any Gemini traffic through the proxy gets tokens
-- counted but dollars null because model_pricing has no Gemini rows.
--
-- Rates are Google list pricing (paid tier, text input) as of 2026-08-17:
-- cache_read = 0.1x input across the family. Gemini has NO per-token
-- cache-write surcharge — explicit caching bills hourly STORAGE instead,
-- which a flat per-MTok row cannot represent — so cache_write is seeded at
-- the base input rate and the storage fee is simply not counted (costs are
-- undervalued, never overvalued). Pro models: short-context (≤200K) tier
-- only, same undervaluation direction as the OpenAI >272K tier in 0004.
-- gemini-3.7/3.6-flash rates are promotional through 2026-12-31.
-- KEEP IN SYNC with store.py _PRICING_SEED.

INSERT INTO model_pricing
    (model, input_per_mtok, output_per_mtok, cache_read_per_mtok, cache_write_per_mtok)
VALUES
    ('gemini-3.7-flash',       0.75,  3.75, 0.075, 0.75),
    ('gemini-3.6-flash',       0.75,  3.75, 0.075, 0.75),
    ('gemini-3.5-flash',       1.5,   9.0,  0.15,  1.5),
    ('gemini-3.5-flash-lite',  0.3,   2.5,  0.03,  0.3),
    ('gemini-3.1-pro-preview', 2.0,  12.0,  0.2,   2.0),
    ('gemini-3.1-flash-lite',  0.25,  1.5,  0.025, 0.25),
    ('gemini-2.5-pro',         1.25, 10.0,  0.125, 1.25),
    ('gemini-2.5-flash',       0.3,   2.5,  0.03,  0.3),
    ('gemini-2.5-flash-lite',  0.1,   0.4,  0.01,  0.1)
ON CONFLICT (model) DO NOTHING;
