"""Savings-ledger accounting math over synthetic savings-ledger/v0 rows.

The load-bearing invariant (§8.4 measurement honesty): tokens_saved sums
counterfactual − billed-full-input over PROBED rows only; a null-probe row is
EXCLUDED from the savings claim (never estimated) while still counting toward
the billed totals.
"""

from __future__ import annotations

import json

from parsec_bench.ledger import (
    LedgerTotals,
    read_ledger,
    rows_for_run,
    served_input_tokens,
    summarize,
    summarize_path,
    to_call_usage,
)


def row(**over) -> dict:
    """A schema-shaped savings-ledger/v0 row with overridable fields."""
    base = {
        "contract_version": "savings-ledger/v0",
        "request_id": "req_" + "0" * 32,
        "ts": "2026-07-10T00:00:00Z",
        "conv_id": "abc123",
        "counterfactual_input_tokens": 1000,
        "billed_input_tokens": 300,
        "billed_output_tokens": 40,
        "billed_cache_read_tokens": 400,
        "billed_cache_write_tokens": 100,
        "cachePrefixSha8": "deadbeef",
        "fail_open": False,
    }
    base.update(over)
    return base


# ── per-row helpers ───────────────────────────────────────────────────────────
def test_served_input_is_full_billable_partition():
    # uncached + cache-read + cache-write, the Anthropic usage partition
    assert served_input_tokens(row()) == 300 + 400 + 100


def test_to_call_usage_shape_for_pricing():
    u = to_call_usage(row())
    assert u == {
        "prompt_tokens": 800,
        "completion_tokens": 40,
        "cache_creation_input_tokens": 100,
        "cache_read_input_tokens": 400,
    }


# ── §8.4: null-probe rows EXCLUDED from tokens_saved ──────────────────────────
def test_null_probe_rows_excluded_from_savings():
    rows = [
        row(counterfactual_input_tokens=1000),   # probed: saved 1000-800 = 200
        row(counterfactual_input_tokens=None,    # probe failed: billed counts,
            billed_input_tokens=999),            # savings contribution ZERO
        row(counterfactual_input_tokens=900,     # probed: saved 900-800 = 100
            request_id="req_" + "1" * 32),
    ]
    t = summarize(rows)
    assert t.requests == 3
    assert t.probed_requests == 2
    assert t.null_probe_requests == 1
    assert t.counterfactual_input_tokens == 1000 + 900
    assert t.served_input_probed == 800 + 800
    assert t.tokens_saved == 200 + 100
    # billed totals still cover ALL rows, incl. the null-probe one
    assert t.billed_input_tokens == 300 + 999 + 300
    assert t.served_input_tokens == 800 + (999 + 400 + 100) + 800
    assert t.billed_output_tokens == 120


def test_all_null_probes_means_zero_savings_not_estimated():
    rows = [row(counterfactual_input_tokens=None) for _ in range(3)]
    t = summarize(rows)
    assert t.tokens_saved == 0
    assert t.counterfactual_input_tokens == 0
    assert t.probed_requests == 0
    assert t.null_probe_requests == 3
    assert t.savings_rate == 0.0


def test_negative_savings_kept_signed():
    # a proxy that ADDS overhead must show negative savings, not clamp to 0
    t = summarize([row(counterfactual_input_tokens=700)])  # served 800
    assert t.tokens_saved == -100


def test_savings_rate():
    t = summarize([row(counterfactual_input_tokens=1000)])
    assert abs(t.savings_rate - 200 / 1000) < 1e-12


# ── capture-seam fields ───────────────────────────────────────────────────────
def test_capture_seam_aggregation():
    ck = "a" * 64
    rows = [
        row(fail_open=True),
        row(scorer_fail_opens=2, brain_ms=12.5, freeze_cut_tokens=340,
            tools_total=12, tools_kept=5, checkpoint_id=ck),
        row(brain_ms=7.5, freeze_cut_tokens=60,
            tools_total=12, tools_kept=4, checkpoint_id=ck),
    ]
    t = summarize(rows)
    assert t.fail_opens == 1
    assert t.scorer_fail_opens == 2
    assert abs(t.brain_ms - 20.0) < 1e-9
    assert t.freeze_cut_tokens == 400          # diagnostic, never a claim
    assert t.tools_total == 12                 # pre-prune roster (max seen)
    assert t.tools_kept == 4                   # forwarded roster (min seen)
    assert t.checkpoint_ids == [ck]            # deduped


def test_old_rows_without_seam_fields_are_fine():
    t = summarize([row()])  # no checkpoint_id/brain_ms/tools_* at all
    assert t.tools_total == 0 and t.tools_kept == 0
    assert t.checkpoint_ids == [] and t.brain_ms == 0.0


# ── file reading + run attribution ────────────────────────────────────────────
def test_read_ledger_skips_garbage_and_missing(tmp_path):
    p = tmp_path / "ledger.jsonl"
    assert read_ledger(p) == []                 # missing file
    p.write_text(json.dumps(row()) + "\n"
                 + "{truncated\n"               # crash tail — skipped
                 + "\n"
                 + json.dumps(row(request_id="req_" + "2" * 32)) + "\n")
    rows = read_ledger(p)
    assert len(rows) == 2


def test_rows_for_run_filters_by_conv_prefix():
    rows = [
        row(conv_id="runA:" + "c" * 24),
        row(conv_id="runB:" + "c" * 24),
        row(conv_id="c" * 24),                  # no run header -> unprefixed
    ]
    assert len(rows_for_run(rows, "runA")) == 1
    assert len(rows_for_run(rows, "runB")) == 1
    assert len(rows_for_run(rows, "")) == 3     # per-run isolated ledger: keep all


def test_summarize_path_end_to_end(tmp_path):
    p = tmp_path / "ledger.jsonl"
    p.write_text(json.dumps(row(conv_id="r1:x")) + "\n"
                 + json.dumps(row(conv_id="r2:x",
                                  counterfactual_input_tokens=None)) + "\n")
    t = summarize_path(p, run_id="r1")
    assert isinstance(t, LedgerTotals)
    assert t.requests == 1 and t.tokens_saved == 200
