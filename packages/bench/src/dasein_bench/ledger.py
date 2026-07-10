"""Savings-ledger accounting — reads the dasein proxy's ~/.dasein/ledger.jsonl.

The NEW proxy (packages/proxy) writes one `savings-ledger/v0` row per completed
/v1/messages request (contracts/schemas/savings-ledger.schema.json):

    contract_version, request_id, ts, conv_id,
    counterfactual_input_tokens,          # count_tokens probe on the ORIGINAL
                                          # body; null = probe failed (never
                                          # estimated — §8.4)
    billed_input_tokens,                  # uncached portion the upstream billed
    billed_output_tokens,
    billed_cache_read_tokens,
    billed_cache_write_tokens,
    cachePrefixSha8, fail_open,
    # capture seams (present only when the real-scorer path ran):
    checkpoint_id, brain_ms, scorer_fail_opens, freeze_cut_tokens,
    tools_total, tools_kept, tools_pre_prune_sha8

MEASUREMENT HONESTY (§8.4, CLAUDE.md): savings numbers come ONLY from the
per-request count_tokens counterfactual — never a modeled baseline. Rows whose
probe failed (counterfactual_input_tokens is null) are EXCLUDED from
tokens_saved entirely; they still contribute to the billed totals.
`freeze_cut_tokens` is a chars/4 diagnostic, NEVER a savings claim.

Per-row served (billed) full input = billed_input_tokens
                                   + billed_cache_read_tokens
                                   + billed_cache_write_tokens
(the same partition Anthropic's usage object reports), so per probed row:

    tokens_saved_row = counterfactual_input_tokens − served_full_input

Signed on purpose: a proxy that adds overhead shows NEGATIVE savings.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional


CONTRACT_VERSION = "savings-ledger/v0"


def read_ledger(path: str | Path) -> list[dict]:
    """Parse a proxy ledger JSONL into rows (call order). Missing file -> [].

    Unparseable lines (a truncated tail from a crash) are skipped, matching the
    resume-ledger convention.
    """
    p = Path(path)
    rows: list[dict] = []
    if not p.exists():
        return rows
    for line in p.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(row, dict):
            rows.append(row)
    return rows


def rows_for_run(rows: list[dict], run_id: str) -> list[dict]:
    """The rows belonging to one bench run.

    The runner sends `x-ccb-run-id: <run_id>` on every Claude Code request
    (ANTHROPIC_CUSTOM_HEADERS); the proxy prefixes its conv_id with
    `"{run_id}:"` so concurrent runs sharing one ledger never mix. An empty
    run_id returns all rows (a per-run isolated ledger file needs no filter).
    """
    if not run_id:
        return list(rows)
    prefix = run_id + ":"
    return [r for r in rows if str(r.get("conv_id") or "").startswith(prefix)]


def served_input_tokens(row: dict) -> int:
    """Billed FULL input for one row: uncached + cache-read + cache-write."""
    def g(k: str) -> int:
        v = row.get(k)
        return int(v or 0)
    return (g("billed_input_tokens")
            + g("billed_cache_read_tokens")
            + g("billed_cache_write_tokens"))


def to_call_usage(row: dict) -> dict:
    """One ledger row -> the CallUsage shape dasein_bench.pricing consumes.

    prompt_tokens is the FULL billable input (the pricing contract); the cache
    split rides through so real_cache_cost prices the row exactly.
    """
    return {
        "prompt_tokens": served_input_tokens(row),
        "completion_tokens": int(row.get("billed_output_tokens") or 0),
        "cache_creation_input_tokens": int(row.get("billed_cache_write_tokens") or 0),
        "cache_read_input_tokens": int(row.get("billed_cache_read_tokens") or 0),
    }


@dataclass
class LedgerTotals:
    """Rollup of one run's proxy-ledger rows.

    Billed totals cover ALL rows; the counterfactual/savings fields cover only
    the PROBED rows (counterfactual_input_tokens non-null) — §8.4.
    """

    requests: int = 0
    # billed (all rows)
    billed_input_tokens: int = 0          # uncached portion
    billed_output_tokens: int = 0
    billed_cache_read_tokens: int = 0
    billed_cache_write_tokens: int = 0
    served_input_tokens: int = 0          # full input = uncached + read + write
    # counterfactual (probed rows only)
    probed_requests: int = 0
    null_probe_requests: int = 0
    counterfactual_input_tokens: int = 0  # Σ probe over probed rows
    served_input_probed: int = 0          # Σ full billed input over probed rows
    tokens_saved: int = 0                 # counterfactual − served, probed only
    # reliability / capture seams
    fail_opens: int = 0                   # rows served passthrough after an error
    scorer_fail_opens: int = 0
    brain_ms: float = 0.0
    freeze_cut_tokens: int = 0            # chars/4 DIAGNOSTIC, never a claim
    tools_total: int = 0                  # max roster seen (pre-prune)
    tools_kept: int = 0                   # min forwarded roster seen
    checkpoint_ids: list[str] = field(default_factory=list)

    @property
    def savings_rate(self) -> float:
        """tokens_saved / counterfactual over probed rows, in [-inf, 1]."""
        if not self.counterfactual_input_tokens:
            return 0.0
        return self.tokens_saved / self.counterfactual_input_tokens


def summarize(rows: list[dict]) -> LedgerTotals:
    """Fold a run's ledger rows into LedgerTotals (§8.4 exclusion applied)."""
    t = LedgerTotals()
    tools_kept_min: Optional[int] = None
    for row in rows:
        t.requests += 1
        t.billed_input_tokens += int(row.get("billed_input_tokens") or 0)
        t.billed_output_tokens += int(row.get("billed_output_tokens") or 0)
        t.billed_cache_read_tokens += int(row.get("billed_cache_read_tokens") or 0)
        t.billed_cache_write_tokens += int(row.get("billed_cache_write_tokens") or 0)
        served = served_input_tokens(row)
        t.served_input_tokens += served

        cf = row.get("counterfactual_input_tokens")
        if cf is None:
            # probe failed: the row is billed but contributes NOTHING to the
            # savings claim — measurement honesty forbids estimating (§8.4).
            t.null_probe_requests += 1
        else:
            cf = int(cf)
            t.probed_requests += 1
            t.counterfactual_input_tokens += cf
            t.served_input_probed += served
            t.tokens_saved += cf - served

        if row.get("fail_open"):
            t.fail_opens += 1
        t.scorer_fail_opens += int(row.get("scorer_fail_opens") or 0)
        t.brain_ms += float(row.get("brain_ms") or 0.0)
        t.freeze_cut_tokens += int(row.get("freeze_cut_tokens") or 0)
        if row.get("tools_total") is not None:
            t.tools_total = max(t.tools_total, int(row["tools_total"]))
        if row.get("tools_kept") is not None:
            k = int(row["tools_kept"])
            tools_kept_min = k if tools_kept_min is None else min(tools_kept_min, k)
        ck = row.get("checkpoint_id")
        if ck and ck not in t.checkpoint_ids:
            t.checkpoint_ids.append(str(ck))
    if tools_kept_min is not None:
        t.tools_kept = tools_kept_min
    return t


def summarize_path(path: str | Path, run_id: str = "") -> LedgerTotals:
    """Convenience: read + filter + summarize a proxy ledger file."""
    return summarize(rows_for_run(read_ledger(path), run_id))
