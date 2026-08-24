-- Platform schema v6 — pricing rows for model strings observed in the live
-- ledger that miss the exact-match pricing join (dollars null, §8.4 hole):
--
--   claude-opus-5             ~120k rows: Opus 5 launched after the 0002 seed;
--                             list price matches Opus 4.x ($5/$25, cache_read
--                             0.1x, cache_write 1.25x — confirmed 2026-08-24).
--   gemini-flash              ~21k rows: bare "latest flash" alias stamped by
--                             clients; priced at the model it currently
--                             resolves to (gemini-3.7-flash, whose rates are
--                             identical to 3.6-flash). Re-check when Google
--                             re-points the alias.
--   claude-haiku-4-5-20251001 ~5k rows: dated snapshot id; same rates as the
--                             claude-haiku-4-5 row from 0002.
--
-- KEEP IN SYNC with store.py _PRICING_SEED.

INSERT INTO model_pricing
    (model, input_per_mtok, output_per_mtok, cache_read_per_mtok, cache_write_per_mtok)
VALUES
    ('claude-opus-5',             5.0,  25.0, 0.5,   6.25),
    ('gemini-flash',              0.75,  3.75, 0.075, 0.75),
    ('claude-haiku-4-5-20251001', 1.0,   5.0,  0.1,   1.25)
ON CONFLICT (model) DO NOTHING;
