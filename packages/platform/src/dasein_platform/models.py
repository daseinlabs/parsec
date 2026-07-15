"""Pydantic mirrors of the contracts schemas the platform ingests.

The JSON Schema files under packages/contracts/schemas/ are authoritative
(single source of truth across packages and languages); these models mirror
savings-ledger.schema.json for request validation, and a test asserts the
model accepts the schema's committed example so drift is caught in CI.
"""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, ConfigDict, Field


class LedgerRow(BaseModel):
    """One row per request — contracts/schemas/savings-ledger.schema.json.

    §8.4 measurement honesty: counterfactual_input_tokens comes from the free
    count_tokens probe on the original body; billed_* from the upstream usage
    block of the same request. extra="forbid" mirrors the schema's
    additionalProperties: false.
    """

    model_config = ConfigDict(extra="forbid", populate_by_name=True)

    contract_version: Literal["savings-ledger/v0"]
    request_id: str = Field(pattern=r"^req_[0-9a-f]{32}$")
    ts: str  # RFC 3339; SQLite stores it as text, Postgres as timestamptz later.
    conv_id: str = Field(max_length=128)
    # null = the probe failed on that request; we record the hole rather than
    # estimate (§8.4). Summaries must skip null rows, never impute.
    counterfactual_input_tokens: int | None = Field(ge=0)
    billed_input_tokens: int = Field(ge=0)
    billed_output_tokens: int = Field(ge=0)
    billed_cache_read_tokens: int = Field(ge=0)
    billed_cache_write_tokens: int = Field(ge=0)
    cache_prefix_sha8: str = Field(alias="cachePrefixSha8", pattern=r"^[0-9a-f]{8}$")
    fail_open: bool

    # ── optional capture seams (real-scorer path; schema-optional) ──────────
    # extra="forbid" + a mirror missing these meant every capture-seam row
    # 422'd at ingest — caught by the governor-example drift guard.
    model: str | None = None
    checkpoint_id: str | None = Field(default=None, pattern=r"^[0-9a-f]{64}$")
    brain_ms: float | None = Field(default=None, ge=0)
    scorer_fail_opens: int | None = Field(default=None, ge=0)
    freeze_cut_tokens: int | None = Field(default=None, ge=0)
    tools_total: int | None = Field(default=None, ge=0)
    tools_kept: int | None = Field(default=None, ge=0)
    tools_pre_prune_sha8: str | None = Field(default=None, pattern=r"^[0-9a-f]{8}$")
    tools_unfrozen: int | None = Field(default=None, ge=0)
    curator_insists: int | None = Field(default=None, ge=0)

    # ── optional governor seams (savings-ledger delta 4; schema-optional) ───
    governor_mode: Literal["advise", "on"] | None = None
    gov_runaway_factor: float | None = Field(default=None, ge=0)
    gov_loop_frac: float | None = Field(default=None, ge=0)
    gov_doom_q: int | None = Field(default=None, ge=0)
    gov_n_src: int | None = Field(default=None, ge=0)
    gov_cum_tok: float | None = Field(default=None, ge=0)
    gov_rule_fires: int | None = Field(default=None, ge=0)
    gov_directive_injected: bool | None = None
    nbr_cost_median: float | None = None
