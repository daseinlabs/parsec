"""On-disk record schema for the benchmark.

Port of adaptive-context-clean/bench/schema.py, extended with the savings
fields the dasein proxy's ledger provides (see dasein_bench.ledger).

A run is one (instance, arm) solve: the fixed scaffold attempts one SWE-bench
task under one compression arm. `RunRecord` captures everything the runner
writes per solve; the ledger is one JSON object per line (resume-safe), and
`AggResult` is the per-arm rollup the report layer consumes.

Pure stdlib dataclasses + a TypedDict mirror for the JSON shape. Token/cost
fields line up with dasein_bench.pricing.CostBreakdown so a record can be
priced and its dollar fields filled in one pass.
"""

from __future__ import annotations

import time
from dataclasses import asdict, dataclass, field
from typing import Optional, TypedDict


# ── per-call usage series (drives cache-aware pricing) ──────────────────────
class CallUsage(TypedDict, total=False):
    """One assistant call's token usage, in call order within a run.

    Captured straight from the model-API usage object so pricing reads the
    REAL cache split (no inference) whenever the provider reports it.

    prompt_tokens               : full input the model saw on this call (billable
                                  input = uncached new + cache-write + cache-read)
    completion_tokens           : tokens it produced
    cache_creation_input_tokens : tokens billed at the cache-WRITE rate
    cache_read_input_tokens     : tokens billed at the cache-READ rate
    latency_s                   : wall-clock seconds for this single call
    model                       : served model id for THIS call (subagent calls
                                  may differ from the run's --model)
    """

    prompt_tokens: int
    completion_tokens: int
    cache_creation_input_tokens: int
    cache_read_input_tokens: int
    latency_s: float
    model: str


# ── one (instance, arm) solve ───────────────────────────────────────────────
@dataclass
class RunRecord:
    """A single graded solve under one arm.

    Core: instance / arm / success / ftp / token sums / calls / wall_s /
    cost_usd (cache-aware $ from the gateway usage series) / patch.

    Outcome: pass_to_pass_ok (no regression), limit_death (hit a cap without
    ever submitting — productive death, distinct from a graded wrong answer).

    Cost frames: cost_usd (cache-aware headline), cost_usd_list (list-price
    upper bound), reported_cost_usd (the SDK's self-reported spend, diagnostic).

    Savings (§8.4 — dasein-arm only, read from the proxy's savings ledger):
      counterfactual_input_tokens : Σ count_tokens probe over PROBED requests
                                    (None when the arm wrote no ledger)
      tokens_saved                : Σ (counterfactual − billed full input) over
                                    probed requests ONLY. Null-probe rows are
                                    EXCLUDED — never estimated. May be negative.
      probed_requests / null_probe_requests : how many rows had / lacked a probe
      proxy_fail_opens            : requests served passthrough after an error
      scorer_fail_opens           : birth steps left undecided by scorer errors
      checkpoint_id               : brain bundle sha256 that scored this run
    """

    # ── core ──
    instance: str
    arm: str
    success: bool
    ftp: float
    input_tokens: int
    output_tokens: int
    cache_write_tok: int
    cache_read_tok: int
    calls: int
    wall_s: float
    cost_usd: float
    patch: str = ""

    # ── outcome ──
    pass_to_pass_ok: bool = True
    limit_death: bool = False

    # ── effort / latency ──
    steps: int = 0
    tool_calls: int = 0
    time_to_submit_s: float = 0.0
    mean_call_latency_s: float = 0.0

    # ── tokens (peak + uncached) ──
    max_prompt_tokens: int = 0
    uncached_input_tokens: int = 0

    # ── cache ──
    cache_hit_rate: float = 0.0

    # ── cost (both frames) ──
    cost_usd_list: float = 0.0
    reported_cost_usd: float = 0.0
    cache_write_usd: float = 0.0
    cache_read_usd: float = 0.0
    output_usd: float = 0.0

    # ── savings (§8.4, from the dasein proxy ledger; see dasein_bench.ledger) ──
    counterfactual_input_tokens: Optional[int] = None
    tokens_saved: Optional[int] = None
    probed_requests: int = 0
    null_probe_requests: int = 0
    proxy_fail_opens: int = 0
    scorer_fail_opens: int = 0
    checkpoint_id: str = ""

    # ── reliability ──
    retries: int = 0
    degraded: bool = False

    # ── optional / diagnostic ──
    model: str = ""
    exit_status: str = ""
    usage: list[CallUsage] = field(default_factory=list)  # per-call series (for re-pricing)
    infra_failed: bool = False                            # True => excluded from metrics
    error: str = ""
    ts: float = field(default_factory=time.time)
    # billed gateway requests INCLUDING subagent traffic (calls = parent
    # num_turns only and can hide requests for spawning arms)
    requests: int = 0
    haiku_requests: int = 0

    def to_json(self) -> dict:
        """Plain dict for one ledger line."""
        return asdict(self)

    @classmethod
    def from_json(cls, d: dict) -> "RunRecord":
        """Rebuild from a ledger line, ignoring unknown keys (forward-compatible)."""
        known = {f for f in cls.__dataclass_fields__}  # type: ignore[attr-defined]
        return cls(**{k: v for k, v in d.items() if k in known})


# ── per-arm rollup ───────────────────────────────────────────────────────────
@dataclass
class AggResult:
    """Aggregate metrics for one arm across all its graded runs.

    Built by the report layer from the RunRecords of a single arm (infra
    failures excluded). vs-baseline delta fields are filled by the report once
    the baseline arm is known; positive = this arm SAVES vs baseline.
    """

    # ── core ──
    arm: str
    n: int                       # graded runs included
    n_success: int               # runs the grader passed
    success_rate: float          # n_success / n

    input_tokens: int
    output_tokens: int
    cache_write_tok: int
    cache_read_tok: int

    cost_usd: float              # cache-aware total $ across the arm
    cost_usd_flat: float         # list-price upper bound across the arm
    cost_per_solved_usd: float   # cost_usd / max(n_success, 1)

    mean_calls: float
    mean_wall_s: float
    total_wall_s: float

    # ── outcome / effort ──
    limit_death_rate: float = 0.0
    mean_steps: float = 0.0
    mean_time_to_submit_s: float = 0.0
    mean_max_prompt_tokens: float = 0.0

    # ── cache (token-weighted) ──
    cache_hit_rate: float = 0.0
    cache_write_usd: float = 0.0
    cache_read_usd: float = 0.0
    output_usd: float = 0.0

    # ── savings (§8.4 sums over probed requests only) ──
    counterfactual_input_tokens: int = 0
    tokens_saved: int = 0
    probed_requests: int = 0
    null_probe_requests: int = 0

    # ── cost summaries ──
    median_cost_usd: float = 0.0
    cost_per_success: float = 0.0   # alias of cost_per_solved_usd

    # ── vs-baseline deltas (filled by report) ──
    input_saving_pct: float = 0.0
    output_saving_pct: float = 0.0
    cost_saving_pct: float = 0.0

    def to_json(self) -> dict:
        return asdict(self)
