"""Pricing math: rate resolution, both cost frames, model-split pricing."""

from __future__ import annotations

from parsec_bench.pricing import (
    DEFAULT_RATES,
    PRICE_TABLE,
    cache_frame_cost,
    has_real_cache_fields,
    price_run,
    rates_for,
    real_cache_cost,
)


# ── rate resolution ───────────────────────────────────────────────────────────
def test_rates_for_longest_substring_match():
    assert rates_for("claude-sonnet-4-6") == PRICE_TABLE["claude-sonnet"]
    assert rates_for("us.anthropic.claude-haiku-4-5") == PRICE_TABLE["claude-haiku"]
    # longest match wins: gpt-4o-mini over gpt-4o
    assert rates_for("gpt-4o-mini-2024") == PRICE_TABLE["gpt-4o-mini"]


def test_rates_for_fallback():
    assert rates_for("mystery-model-9000") == DEFAULT_RATES


def test_rates_table_anthropic_rows_exact():
    # ported from the reference table as-is — a silent price edit is a bug
    assert PRICE_TABLE["claude-sonnet"] == {
        "input": 3.0, "cache_write": 3.75, "cache_read": 0.30, "output": 15.0}
    assert PRICE_TABLE["claude-opus"] == {
        "input": 5.0, "cache_write": 6.25, "cache_read": 0.50, "output": 25.0}
    assert PRICE_TABLE["claude-haiku"] == {
        "input": 1.0, "cache_write": 1.25, "cache_read": 0.10, "output": 5.0}


# ── real-cache frame ──────────────────────────────────────────────────────────
def test_real_cache_cost_partitions_and_prices():
    rates = rates_for("claude-sonnet")
    usage = [
        # prompt = uncached 200 + write 500 + read 300
        {"prompt_tokens": 1000, "completion_tokens": 100,
         "cache_creation_input_tokens": 500, "cache_read_input_tokens": 300},
        # prompt = uncached 0 + write 100 + read 800
        {"prompt_tokens": 900, "completion_tokens": 50,
         "cache_creation_input_tokens": 100, "cache_read_input_tokens": 800},
    ]
    cb = real_cache_cost(usage, rates)
    assert cb.cache_write_tok == 600
    assert cb.cache_read_tok == 1100
    assert cb.uncached_input_tok == 200
    assert cb.output_tok == 150
    assert abs(cb.write_usd - 600 * 3.75 / 1e6) < 1e-12
    assert abs(cb.read_usd - 1100 * 0.30 / 1e6) < 1e-12
    assert abs(cb.uncached_usd - 200 * 3.0 / 1e6) < 1e-12
    assert abs(cb.output_usd - 150 * 15.0 / 1e6) < 1e-12
    assert abs(cb.total_usd - (cb.write_usd + cb.read_usd + cb.uncached_usd + cb.output_usd)) < 1e-15
    # list frame: ALL input at the base input rate
    assert abs(cb.list_usd - (1900 * 3.0 / 1e6 + 150 * 15.0 / 1e6)) < 1e-12
    assert abs(cb.cache_hit_rate - 1100 / 1900) < 1e-12


def test_real_cache_cost_clamps_noisy_split():
    # over-counted split must never yield a negative uncached bucket
    cb = real_cache_cost([{"prompt_tokens": 100, "completion_tokens": 0,
                           "cache_creation_input_tokens": 80,
                           "cache_read_input_tokens": 50}], DEFAULT_RATES)
    assert cb.uncached_input_tok == 0


# ── inferred-growth frame ─────────────────────────────────────────────────────
def test_cache_frame_cost_growth_is_write_prefix_is_read():
    rates = {"cache_write": 4.0, "cache_read": 1.0, "output": 10.0, "input": 2.0}
    cb = cache_frame_cost([100, 150, 150], [10, 10, 10], rates)
    # writes: 100 + 50 + 0 ; reads: 0 + 100 + 150
    assert cb.cache_write_tok == 150
    assert cb.cache_read_tok == 250
    assert cb.output_tok == 30
    assert abs(cb.write_usd - 150 * 4.0 / 1e6) < 1e-12
    assert abs(cb.read_usd - 250 * 1.0 / 1e6) < 1e-12
    assert abs(cb.list_usd - ((150 + 250) * 2.0 / 1e6 + 30 * 10.0 / 1e6)) < 1e-12


def test_cache_frame_cost_shrink_default_allowance():
    rates = {"cache_write": 4.0, "cache_read": 1.0, "output": 10.0}
    # prompt shrinks 200 -> 120: default treats 50 as fresh (write), 70 read
    cb = cache_frame_cost([200, 120], [0, 0], rates)
    assert cb.cache_write_tok == 200 + 50
    assert cb.cache_read_tok == 0 + 70


def test_cache_frame_cost_shrink_hook():
    rates = {"cache_write": 4.0, "cache_read": 1.0, "output": 10.0}
    cb = cache_frame_cost([200, 120], [0, 0], rates,
                          shrink_fresh_tokens=lambda i: 10)
    assert cb.cache_write_tok == 200 + 10
    assert cb.cache_read_tok == 0 + 110


def test_rate_fallback_input_to_cache_write():
    # 3-key override: uncached prices at cache_write, not silently 0
    rates = {"cache_write": 4.0, "cache_read": 1.0, "output": 10.0}
    cb = real_cache_cost([{"prompt_tokens": 100, "completion_tokens": 0}], rates)
    assert abs(cb.uncached_usd - 100 * 4.0 / 1e6) < 1e-12


# ── entry point: prefer real fields, split by served model ────────────────────
def test_price_run_prefers_real_fields():
    rates = rates_for("claude-sonnet")
    usage = [{"prompt_tokens": 100, "completion_tokens": 1,
              "cache_read_input_tokens": 0}]  # key PRESENCE selects real path
    cb = price_run(usage, rates)
    assert cb.uncached_input_tok == 100        # real path has an uncached bucket


def test_price_run_falls_back_to_growth():
    rates = rates_for("claude-sonnet")
    usage = [{"prompt_tokens": 100, "completion_tokens": 1},
             {"prompt_tokens": 160, "completion_tokens": 1}]
    assert not has_real_cache_fields(usage)
    cb = price_run(usage, rates)
    assert cb.uncached_input_tok == 0          # growth path: write absorbs new input
    assert cb.cache_write_tok == 160
    assert cb.cache_read_tok == 100


def test_price_run_prices_each_served_model_at_its_own_rates():
    usage = [
        {"prompt_tokens": 1_000_000, "completion_tokens": 0,
         "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
         "model": "claude-sonnet-4-6"},
        {"prompt_tokens": 1_000_000, "completion_tokens": 0,
         "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
         "model": "claude-haiku-4-5"},
    ]
    cb = price_run(usage, rates_for("claude-sonnet"))
    # 1 MTok uncached sonnet ($3) + 1 MTok uncached haiku ($1)
    assert abs(cb.total_usd - 4.0) < 1e-9
    assert cb.uncached_input_tok == 2_000_000
