"""Pydantic mirrors of the contracts schemas the platform ingests.

The JSON Schema files under packages/contracts/schemas/ are authoritative
(single source of truth across packages and languages); these models mirror
savings-ledger.schema.json for request validation, and a test asserts the
model accepts the schema's committed example so drift is caught in CI.
"""

from __future__ import annotations

from typing import Annotated, Literal

from pydantic import BaseModel, ConfigDict, Field, NonNegativeInt

# The internal view's closed role set (proxy/src/protect.rs `role_key`). The
# proxy buckets anything off this list, so an inbound role string can never
# reach the ledger as a key.
ProtectRole = Literal[
    "system", "user", "assistant", "tool", "opaque", "(other)", "(unrecorded)"
]


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
    # Client harness session identity (an id, never content) — groups the
    # conv_ids one session mints via compaction/subagents.
    session_id: str | None = Field(default=None, pattern=r"^[0-9a-fA-F-]{1,64}$")
    # Calling-tool attribution (x-parsec-tool header from non-Claude-Code
    # shims, e.g. opencode). The proxy now stamps "claude-code" explicitly on
    # untagged Anthropic-wire requests; absent = an older row or an untagged
    # wire — readers coalesce absent to claude-code.
    tool: str | None = Field(default=None, pattern=r"^[a-z0-9-]{1,32}$")
    # The machine's anonymous install id (install-report/v0) — joins ledger
    # rows to the installs fleet view. Absent until the install minted one.
    install_id: str | None = Field(default=None, pattern=r"^ins_[0-9a-f]{32}$")
    checkpoint_id: str | None = Field(default=None, pattern=r"^[0-9a-f]{64}$")
    brain_ms: float | None = Field(default=None, ge=0)
    scorer_fail_opens: int | None = Field(default=None, ge=0)
    freeze_cut_tokens: int | None = Field(default=None, ge=0)
    # Which instrument produced counterfactual_input_tokens when it was NOT
    # the provider's count_tokens probe. Absent = provider-probed (the
    # authoritative case); "local_bpe" = our own tokenizer, on a wire that
    # offers no probe. Literal, not str: a reader must never have to guess
    # whether a savings figure is authoritative, so adding an instrument is a
    # deliberate contract change (§8.4).
    counterfactual_source: Literal["local_bpe"] | None = None
    # Per-role cut breakdown, internal-view chars/4 (same units as
    # freeze_cut_tokens, NOT billed tokens). Keys are the internal view's
    # closed role set — the inbound role is copied verbatim into that view,
    # so the gate is what keeps this path unable to carry raw text, exactly
    # like the `tool` and `session_id` patterns above.
    freeze_cut_roles: dict[ProtectRole, NonNegativeInt] | None = None
    # Mass the curator tried to cut from human-authored entries and that the
    # protection guard restored. The over-cut alarm on the Anthropic wire
    # (normally 0); routine on the Responses wire, where the freezer chunks
    # every non-first user turn by design and the guard restores it.
    freeze_cut_protected_tokens: int | None = Field(default=None, ge=0)
    tools_total: int | None = Field(default=None, ge=0)
    tools_kept: int | None = Field(default=None, ge=0)
    tools_pre_prune_sha8: str | None = Field(default=None, pattern=r"^[0-9a-f]{8}$")
    tools_served_sha8: str | None = Field(default=None, pattern=r"^[0-9a-f]{8}$")
    tools_unfrozen: int | None = Field(default=None, ge=0)
    tools_stubbed: int | None = Field(default=None, ge=0)
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


class InstallReport(BaseModel):
    """One registration ping per parsec install —
    contracts/schemas/install-report.schema.json.

    Fleet accounting only: an anonymous client-minted machine id plus coarse
    environment facts. Every field is pattern-gated so this path cannot carry
    raw text (same guarantee as the ledger's tool/session_id fields).
    extra="forbid" mirrors the schema's additionalProperties: false.
    """

    model_config = ConfigDict(extra="forbid")

    contract_version: Literal["install-report/v0"]
    install_id: str = Field(pattern=r"^ins_[0-9a-f]{32}$")
    ts: str  # RFC 3339, like the ledger's ts.
    version: str = Field(pattern=r"^[0-9A-Za-z._+-]{1,32}$")
    os: str = Field(pattern=r"^[a-z0-9_]{1,16}$")
    arch: str = Field(pattern=r"^[a-z0-9_]{1,16}$")
    # Harnesses parsec is CONFIGURED to intercept at report time — the
    # install-level complement of the per-request `tool` field. May be empty
    # (setup not finished).
    harnesses: list[Annotated[str, Field(pattern=r"^[a-z0-9-]{1,32}$")]] = Field(
        max_length=8
    )
