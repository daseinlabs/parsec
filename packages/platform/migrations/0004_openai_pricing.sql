-- Platform schema v4 — OpenAI model pricing for Codex/ChatGPT traffic.
-- The proxy has served OpenAI (Codex) requests since the responses-API port,
-- but model_pricing only seeded Claude models, so those ledger rows missed the
-- report-time pricing join: tokens counted, dollars null (§8.4 hole).
--
-- Rates are OpenAI list pricing as of 2026-08-14 (post the July 30 Terra/Luna
-- cut): cache_read = 0.1x input, cache_write = 1.25x input per the published
-- gpt-5.6 rates. Short-context tier only — the >272K long-context tier bills
-- higher, but a single flat row can't represent it, so long-context savings
-- are undervalued, never overvalued.
-- KEEP IN SYNC with store.py _PRICING_SEED.

INSERT INTO model_pricing
    (model, input_per_mtok, output_per_mtok, cache_read_per_mtok, cache_write_per_mtok)
VALUES
    ('gpt-5.6-sol',   5.0,  30.0,  0.5,   6.25),
    ('gpt-5.6-terra', 2.0,  12.0,  0.2,   2.5),
    ('gpt-5.6-luna',  0.2,   1.2,  0.02,  0.25),
    ('gpt-5.5',       5.0,  30.0,  0.5,   6.25),
    ('gpt-5.4-mini',  0.75,  4.5,  0.075, 0.9375),
    ('gpt-5.4-nano',  0.2,   1.25, 0.02,  0.25),
    ('gpt-5.3-codex', 1.75, 14.0,  0.175, 2.1875),
    ('gpt-5.2-codex', 1.75, 14.0,  0.175, 2.1875),
    ('gpt-5-codex',   1.25, 10.0,  0.125, 1.5625),
    ('gpt-5-mini',    0.25,  2.0,  0.025, 0.3125),
    ('gpt-5-nano',    0.05,  0.4,  0.005, 0.0625)
ON CONFLICT (model) DO NOTHING;
