"""Cache-aware per-model pricing (pure stdlib).

Port of adaptive-context-clean/bench/pricing.py, table and math as-is.

Agent runs are long multi-turn loops where the prompt grows monotonically: each
call re-sends the whole prior transcript plus the newest turn. With prompt
caching, that re-sent prefix is billed at the cheap CACHE-READ rate and only the
newly-appended tokens are billed at the expensive CACHE-WRITE rate; output is
billed flat. Pricing a run at a single flat input rate massively overstates the
bill, so we reconstruct the cache frame from the per-call usage series.

PRICE_TABLE is $/MTok. Rates are seeded for common Sonnet/Opus/Haiku and a few
OpenAI/Gemini rows; override or extend at the call site. All figures are public
list-price approximations — confirm against your provider's current rates.

Two pricing paths, in priority order:

1. REAL cache fields (preferred). When the provider reports the cache split per
   call — usage.cache_creation_input_tokens (the WRITE) and
   usage.cache_read_input_tokens (the READ) — we price exactly what the bill
   says: writes at the cache-write rate, reads at the cache-read rate, the
   remaining uncached input (prompt_tokens − write − read) at the full input
   rate, and completion flat. No inference. See `real_cache_cost()`.

2. INFERRED-from-prompt-growth (FALLBACK). When a call lacks the cache fields:
   per call, the GROWTH in prompt_tokens over the prior call is priced as a
   write, the prior prefix as a read, and completion flat. A prompt that
   SHRINKS (a compression arm trims the transcript) is handled by the
   `shrink_fresh_tokens` hook so the dropped prefix is not mis-billed as a
   write. See `cache_frame_cost()`.

The LIST-frame cost (`list_usd`) is the naive upper bound: every input token at
the full input rate (no cache credit) + output flat — the "what a non-caching
bill would be" ceiling reported alongside the cache-aware figure.

NB (§8.4 measurement honesty): these dollar frames price what was BILLED. They
are never a savings baseline; savings numbers come only from the per-request
count_tokens counterfactual in the proxy's ledger (see parsec_bench.ledger).
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Callable, Iterable, Optional


# ── price table: $/MTok = dollars per million tokens ────────────────────────
# input       : uncached / first-seen input tokens at the BASE input rate
# cache_write : freshly-cached input tokens (Anthropic: 1.25x the input rate)
# cache_read  : cached prefix re-sent on a later call
# output      : completion tokens (flat)
#
# Back-compat: a row may omit "input"; `_rate(rates, "input")` then falls back
# to "cache_write" so a 3-key external override still prices correctly.
PRICE_TABLE: dict[str, dict[str, float]] = {
    # Anthropic Claude (list price, $/MTok)
    "claude-sonnet": {"input": 3.0,  "cache_write": 3.75, "cache_read": 0.30, "output": 15.0},
    "claude-opus":   {"input": 5.0,  "cache_write": 6.25, "cache_read": 0.50, "output": 25.0},
    "claude-haiku":  {"input": 1.0,  "cache_write": 1.25, "cache_read": 0.10, "output": 5.0},
    # OpenAI (approximate; confirm current rates)
    "gpt-4o":        {"input": 2.50, "cache_write": 2.50, "cache_read": 1.25, "output": 10.0},
    "gpt-4o-mini":   {"input": 0.15, "cache_write": 0.15, "cache_read": 0.075, "output": 0.60},
    # Google Gemini (approximate; implicit caching)
    "gemini-1.5-pro":   {"input": 1.25,  "cache_write": 1.25,  "cache_read": 0.3125,  "output": 5.0},
    "gemini-1.5-flash": {"input": 0.075, "cache_write": 0.075, "cache_read": 0.01875, "output": 0.30},
}

# Fallback used when a model id matches no table row (and no override given).
DEFAULT_RATES: dict[str, float] = {
    "input": 3.0, "cache_write": 3.75, "cache_read": 0.30, "output": 15.0,
}


def _rate(rates: dict[str, float], key: str) -> float:
    """Look up a $/MTok rate, with input -> cache_write back-compat fallback."""
    if key in rates:
        return rates[key]
    if key == "input":
        return rates.get("cache_write", DEFAULT_RATES["input"])
    return rates.get(key, 0.0)


@dataclass
class CostBreakdown:
    """Dollar decomposition of one run plus the token counts that produced it."""

    write_usd: float          # cache-write tokens, priced at cache_write
    read_usd: float           # re-sent cached prefix, priced at cache_read
    output_usd: float         # completion tokens, priced at output
    cache_write_tok: int      # total tokens billed as writes
    cache_read_tok: int       # total tokens billed as reads
    output_tok: int           # total completion tokens
    # real-cache path only (0 on the inferred-growth path, which has no separate
    # uncached bucket — there, growth==write absorbs the new input):
    uncached_usd: float = 0.0          # uncached input tokens, priced at input rate
    uncached_input_tok: int = 0        # total input tokens billed at the full input rate
    # list-frame upper bound: every input token at the base input rate + output flat
    list_usd: float = 0.0

    @property
    def input_usd(self) -> float:
        """Input bill = writes + reads + uncached (excludes output)."""
        return self.write_usd + self.read_usd + self.uncached_usd

    @property
    def total_usd(self) -> float:
        return self.write_usd + self.read_usd + self.uncached_usd + self.output_usd

    @property
    def cache_hit_rate(self) -> float:
        """Read share of total input work, in [0,1]."""
        denom = self.cache_read_tok + self.cache_write_tok + self.uncached_input_tok
        return self.cache_read_tok / denom if denom else 0.0


def rates_for(model_id: str,
              table: Optional[dict[str, dict[str, float]]] = None) -> dict[str, float]:
    """Resolve {input, cache_write, cache_read, output} for a model id.

    Matching is by longest case-insensitive substring of a table key against the
    model id (so 'claude-sonnet-4-6' resolves to the 'claude-sonnet' row).
    Falls back to DEFAULT_RATES when nothing matches.
    """
    tbl = table if table is not None else PRICE_TABLE
    mid = model_id.lower()
    best_key, best_len = None, -1
    for key in tbl:
        k = key.lower()
        if k in mid and len(k) > best_len:
            best_key, best_len = key, len(k)
    return dict(tbl[best_key]) if best_key is not None else dict(DEFAULT_RATES)


def cache_frame_cost(
    prompt_tokens: Iterable[int],
    completion_tokens: Iterable[int],
    rates: dict[str, float],
    *,
    shrink_fresh_tokens: Optional[Callable[[int], int]] = None,
) -> CostBreakdown:
    """Cache-aware cost inferred from a run's per-call prompt-growth series.

    prompt_tokens / completion_tokens : per-assistant-call usage, in call order.
        prompt_tokens[i] is the full input the model saw on call i.
    rates : $/MTok dict (see rates_for()).
    shrink_fresh_tokens : optional. When call i's prompt is SMALLER than call
        i-1's (a compression layer trimmed the transcript), returns how many of
        call i's tokens are genuinely NEW (a write); the remainder is the
        surviving cached prefix (a read). If omitted, a shrink is treated as:
        everything is a read except a small fixed allowance — conservative,
        never negative.
    """
    pw = _rate(rates, "cache_write") / 1e6
    pr = _rate(rates, "cache_read") / 1e6
    po = _rate(rates, "output") / 1e6
    pin = _rate(rates, "input") / 1e6

    prompts = list(prompt_tokens)
    comps = list(completion_tokens)

    w_tok = r_tok = o_tok = 0
    prev_p = 0
    for i, p in enumerate(prompts):
        ct = comps[i] if i < len(comps) else 0
        o_tok += ct
        if p >= prev_p:
            # prompt grew (the normal monotonic case): new = write, prefix = read
            w_tok += (p - prev_p)
            r_tok += prev_p
        else:
            # prompt shrank: only the genuinely-new tokens are a write
            fresh = shrink_fresh_tokens(i) if shrink_fresh_tokens else 50
            fresh = max(0, min(fresh, p))
            common = p - fresh
            w_tok += fresh
            r_tok += common
        prev_p = p

    # list-frame upper bound: all inferred input work at the base input rate.
    list_usd = (w_tok + r_tok) * pin + o_tok * po
    return CostBreakdown(
        write_usd=w_tok * pw,
        read_usd=r_tok * pr,
        output_usd=o_tok * po,
        cache_write_tok=w_tok,
        cache_read_tok=r_tok,
        output_tok=o_tok,
        uncached_usd=0.0,
        uncached_input_tok=0,
        list_usd=list_usd,
    )


def real_cache_cost(
    usage: Iterable[dict],
    rates: dict[str, float],
) -> CostBreakdown:
    """Price a run from the REAL per-call cache fields the provider reported.

    This is the PREFERRED path: no inference. Each usage dict is one assistant
    call (in call order) carrying prompt_tokens (the FULL billable input:
    uncached new + cache-write + cache-read), completion_tokens, and the cache
    split (cache_creation_input_tokens / cache_read_input_tokens). The
    remaining input — prompt_tokens − write − read — is billed at the base
    input rate; output flat.
    """
    pw = _rate(rates, "cache_write") / 1e6
    pr = _rate(rates, "cache_read") / 1e6
    po = _rate(rates, "output") / 1e6
    pin = _rate(rates, "input") / 1e6

    w_tok = r_tok = u_tok = o_tok = 0
    total_input = 0
    for u in usage:
        pt = int(u.get("prompt_tokens", 0) or 0)
        ct = int(u.get("completion_tokens", 0) or 0)
        cw = int(u.get("cache_creation_input_tokens", 0) or 0)
        crd = int(u.get("cache_read_input_tokens", 0) or 0)
        # the uncached remainder; clamp at 0 so a noisy/over-counted split never
        # produces a negative bucket.
        unc = max(0, pt - cw - crd)
        w_tok += cw
        r_tok += crd
        u_tok += unc
        o_tok += ct
        total_input += pt

    return CostBreakdown(
        write_usd=w_tok * pw,
        read_usd=r_tok * pr,
        output_usd=o_tok * po,
        cache_write_tok=w_tok,
        cache_read_tok=r_tok,
        output_tok=o_tok,
        uncached_usd=u_tok * pin,
        uncached_input_tok=u_tok,
        # list-frame: every input token (the full prompt sum) at the base input rate
        list_usd=total_input * pin + o_tok * po,
    )


def has_real_cache_fields(usage: Iterable[dict]) -> bool:
    """True if ANY call in the series carries a real cache field.

    The signal to prefer real_cache_cost() over the inferred-growth fallback.
    Key PRESENCE is the signal — an all-cold run with the keys reported still
    counts as real; an empty/absent series does not.
    """
    for u in usage:
        if ("cache_creation_input_tokens" in u) or ("cache_read_input_tokens" in u):
            return True
    return False


def price_run(usage: Iterable[dict], rates: dict[str, float]) -> CostBreakdown:
    """Price a run, preferring the REAL cache fields and falling back to inference.

    The single entry point the runner/report should call. When rows carry a
    per-row served `model` (the gateway stamps it; subagent calls may be haiku
    while the run is sonnet), each model's rows are priced at ITS rates and
    summed; rows without a model use the caller's rates.
    """
    series = list(usage)
    if has_real_cache_fields(series):
        models = {str(u.get("model") or "") for u in series}
        if len(models - {""}) >= 1 and models != {""}:
            import dataclasses
            total = None
            for m in sorted(models):
                part = [u for u in series if str(u.get("model") or "") == m]
                r = rates_for(m) if m else dict(rates)
                cb = real_cache_cost(part, r)
                if total is None:
                    total = cb
                else:
                    merged = {}
                    for f in dataclasses.fields(cb):
                        a, b = getattr(total, f.name), getattr(cb, f.name)
                        merged[f.name] = (a + b) if isinstance(a, (int, float)) and not isinstance(a, bool) else a
                    total = dataclasses.replace(total, **merged)
            return total
        return real_cache_cost(series, rates)
    return cache_frame_cost(
        [int(u.get("prompt_tokens", 0) or 0) for u in series],
        [int(u.get("completion_tokens", 0) or 0) for u in series],
        rates,
    )
