"""FastAPI surface for BOTH brain contracts, dispatched on the request's `contract` field:

  brain-api-dev/v0  (contracts/schemas/brain-api-dev.schema.json)
    ⚠️ Dev-raw posture: carries the internal message view (RAW TEXT) — dev/dogfood machines
    only, behind the client's explicit DASEIN_BRAIN_DEV_RAW=1 opt-in.
  brain-api/v1      (contracts/schemas/brain-api.schema.json)
    Data-plane clean: client-computed vectors + structural features + opaque ids in; raw
    text is UNREPRESENTABLE (every string field is an enum, a hex id, or a bounded
    identifier). Requires the request checkpoint_id to match the loaded bundle (409
    otherwise — the §8.2 matched-pair guard: features and weights must move together).

The bundle loads (and self-validates) at app construction: a bad checkpoint refuses to start.

Run: uvicorn --factory dasein_brain.app:create_app --port 8080
"""
from __future__ import annotations

from . import _flags  # noqa: F401  parity pins before bundle/scorer (vendored) imports

import os
import threading
from typing import Annotated, Literal, Union

from fastapi import FastAPI, HTTPException, Request
from pydantic import BaseModel, ConfigDict, Field

from . import v1graph
from .bundle import load_bundle
from .scorer import TraceScorer, chunk_checksum

CONTRACT = "brain-api-dev/v0"
CONTRACT_V1 = "brain-api/v1"
GRID = 1_000_000                     # engine SCORE_SCALE: q = round(score * GRID)
RULE_TAU_HINT_Q = 150_000            # handoff's ~0.15 — ADVISORY ONLY, never a firing threshold
RULE_TAU_DESCRIPTION = (
    "tau_hint_q is advisory (rule-head handoff ~0.15; scores are compressed, ranking is the "
    "signal) — the firing tau MUST be bench-calibrated before any consumer fires on it. No "
    "proxy consumer is wired to this head yet. scores_q == {} means ineligible/failed: fail "
    "open (ungated fire is the baseline).")
GATE_TAU_Q = 500_000                 # gate tau 0.5, locked (reference operating point)
_RULE_DEFAULT_STATUS = ("active", "always_on")   # roster subset served when `rules` is omitted


def _q(x: float) -> int:
    return int(round(float(x) * GRID))


class ScoreTraceRequest(BaseModel):
    contract: Literal["brain-api-dev/v0"]
    conv_id: str = Field(max_length=128)
    messages: list[dict]
    live_gi: list[int]
    mask: list[int]
    cur_step: int
    chunk_checksum: str
    target_cov: str | None = None    # accepted per schema; tau resolves at bundle load, not here


class ScoreToolsRequest(BaseModel):
    contract: Literal["brain-api-dev/v0"]
    conv_id: str = Field(max_length=128)
    messages: list[dict]
    tools: list[dict]


# ---- brain-api/v1: vectors + features in, NO raw text representable --------------------------
# Every string field below is a Literal, a fixed-format hex id, or a bounded identifier
# (conv_id / tool name — harness identifiers, never user code). extra="forbid" everywhere:
# a client cannot smuggle a free-text field past the contract. test_v1.py asserts this
# structurally against the pydantic models AND the JSON schema.
_HEX16 = r"^[0-9a-f]{16}$"
_HEX64 = r"^[0-9a-f]{64}$"
_IDENT = r"^[A-Za-z0-9:._-]{1,128}$"

Vec1024 = Annotated[list[float], Field(min_length=1024, max_length=1024)]


class V1Node(BaseModel):
    """One live chunk, featurized client-side (decision order preserved by the caller)."""
    model_config = ConfigDict(extra="forbid")
    emb_text: Vec1024                # embedding of chunk text[:2000]
    emb_cmd: Vec1024                 # embedding of the producing command; zeros when none
    emb_head: Vec1024                # embedding of the observation head[:240]; zeros when none
    struct: list[float] = Field(min_length=21, max_length=21)   # node_struct_with_type row
    step: int = Field(ge=0)
    kind: Literal["read", "grep", "other", "reasoning", "asst"]
    tokens: int = Field(ge=1)
    file_id: str | None = Field(default=None, pattern=_HEX16)   # salted basename hash
    lo: int | None = None
    hi: int | None = None
    cmd_id: str | None = Field(default=None, pattern=_HEX16)    # salted command hash
    head_id: str | None = Field(default=None, pattern=_HEX16)   # salted head hash (presence
    #                                                             gates the head hub + marker)


class ScoreTraceV1Request(BaseModel):
    model_config = ConfigDict(extra="forbid")
    contract: Literal["brain-api/v1"]
    conv_id: str = Field(pattern=_IDENT)
    checkpoint_id: str = Field(pattern=_HEX64)   # must match the bundle (409 on mismatch)
    cur_step: int = Field(ge=0)
    nodes: list[V1Node]                          # the LIVE chunk set, decision order
    task_emb: Vec1024
    sys_emb: Vec1024 | None = None               # omitted -> zeros (no system prompt)
    mask: list[int]                              # rows of `nodes` to decide
    decided_struct: list[list[float]]            # 49-col readout row per mask entry
    edges_supersession: list[tuple[int, int]]    # client-computed rel-4 (src, dst) pairs —
    #                                              the only text-dependent edge relation
    target_cov: str | None = Field(default=None, pattern=r"^0\.\d{2}$")


class V1Tool(BaseModel):
    model_config = ConfigDict(extra="forbid")
    # tool NAMES ride: they are harness identifiers (the keep-set filter's join key back to
    # the request roster), not user code/text.
    name: str = Field(pattern=_IDENT)
    emb: Vec1024                                 # embedding of the serialized schema text
    tokens: int = Field(ge=1)                    # schema token mass (the prune ranking unit)


class ScoreToolsV1Request(BaseModel):
    model_config = ConfigDict(extra="forbid")
    contract: Literal["brain-api/v1"]
    conv_id: str = Field(pattern=_IDENT)
    checkpoint_id: str = Field(pattern=_HEX64)
    nodes: list[V1Node]                          # tool-spec chunk pipeline view (client-side)
    task_emb: Vec1024
    sys_emb: Vec1024 | None = None
    tools: list[V1Tool]


ScoreTraceBody = Annotated[Union[ScoreTraceRequest, ScoreTraceV1Request],
                           Field(discriminator="contract")]
ScoreToolsBody = Annotated[Union[ScoreToolsRequest, ScoreToolsV1Request],
                           Field(discriminator="contract")]


class ScoreRulesRequest(BaseModel):
    contract: Literal["brain-api-dev/v0"]
    conv_id: str = Field(max_length=128)
    messages: list[dict]
    tools: list[dict]
    rules: list[dict] | None = None  # [{eid, text}]; omitted -> bundle active+always_on roster
    step: int = Field(ge=0)


class ScoreGateRequest(BaseModel):
    contract: Literal["brain-api-dev/v0"]
    conv_id: str = Field(max_length=128)
    messages: list[dict]
    tools: list[dict]
    brief: str


def create_app() -> FastAPI:
    bundle = load_bundle()           # self-validating: any mismatch raises, the app never starts
    scorer = TraceScorer(bundle)
    lock = threading.Lock()          # one CPU forward at a time; scorer caches are shared state
    key = os.environ.get("DASEIN_BRAIN_KEY", "")
    # rules.json subset served when a /v1/score/rules request omits `rules`: the governor's live
    # roster = status active|always_on (candidate/retired rules are never fired unrequested).
    rule_defaults = [{"eid": r["eid"], "text": r["text"]} for r in bundle.rules
                     if r.get("status") in _RULE_DEFAULT_STATUS]
    app = FastAPI(title="dasein-brain", version="0.1.0")

    def _auth(request: Request) -> None:
        if key and request.headers.get("authorization") != f"Bearer {key}":
            raise HTTPException(status_code=401, detail="missing or invalid bearer token")

    @app.get("/health")
    def health():
        return {"status": "ok"}

    @app.get("/v1/bundle")
    def bundle_info(request: Request):
        _auth(request)
        return {
            "contract": CONTRACT,
            "contracts": [CONTRACT, CONTRACT_V1],   # score/trace + score/tools dispatch on both
            "checkpoint_id": bundle.checkpoint_id,
            "tau_q": bundle.tau_q,
            "target_cov": bundle.target_cov,
            "grid": GRID,
            "heads": ["curator", "tool", "rule", "gate"],   # rule/gate: no proxy consumer yet
            "neighbors": False,      # nf=None in v0: +3 zero block-parity cols (trained w/ dropout)
            "flags": bundle.flags,
        }

    def _v1_checkpoint_guard(req_checkpoint_id: str) -> None:
        """§8.2 matched-pair trap: v1 features were computed against a specific checkpoint's
        recipe; scoring them with a different bundle is a silently-wrong-score bug -> 409."""
        if req_checkpoint_id != bundle.checkpoint_id:
            raise HTTPException(status_code=409, detail={
                "error": "checkpoint_id mismatch",
                "server_checkpoint_id": bundle.checkpoint_id,
            })

    def _score_trace_v1(req: ScoreTraceV1Request):
        _v1_checkpoint_guard(req.checkpoint_id)
        n = len(req.nodes)
        if any(j < 0 or j >= n for j in req.mask):
            raise HTTPException(status_code=422, detail=f"mask out of range for {n} nodes")
        if len(req.decided_struct) != len(req.mask):
            raise HTTPException(status_code=422,
                                detail="decided_struct must carry one row per mask entry")
        if any(len(r) != bundle.read_struct for r in req.decided_struct):
            raise HTTPException(status_code=422,
                                detail=f"decided_struct rows must be {bundle.read_struct} wide "
                                       f"(ckpt read_struct)")
        if any(not (0 <= a < n and 0 <= b < n) for (a, b) in req.edges_supersession):
            raise HTTPException(status_code=422,
                                detail=f"edges_supersession out of range for {n} nodes")
        if not req.nodes or not req.mask:            # nothing to decide: all rows never-cut
            return {"scores_q": [GRID] * n, "tau_q": bundle.tau_q,
                    "checkpoint_id": bundle.checkpoint_id,
                    "timings_ms": {"embed": 0.0, "forward": 0.0}}
        with lock:
            sc, tau, timings = v1graph.score_trace(
                bundle, req.nodes, req.task_emb, req.sys_emb, req.mask,
                req.decided_struct, req.edges_supersession)
        return {"scores_q": [_q(s) for s in sc], "tau_q": _q(tau),
                "checkpoint_id": bundle.checkpoint_id, "timings_ms": timings}

    def _score_tools_v1(req: ScoreToolsV1Request):
        _v1_checkpoint_guard(req.checkpoint_id)
        if not req.tools:                            # ineligible: fail-open shape (full roster)
            return {"names": [], "scores_q": [], "tokens": [],
                    "checkpoint_id": bundle.checkpoint_id}
        if not req.nodes:
            # the dev path never scores an empty graph (build_tool_spec falls back to chunking
            # the task text at step 0); the v1 client mirrors that fallback and always sends
            # at least one node — an empty set here is a client featurization bug, not fail-open.
            raise HTTPException(status_code=422,
                                detail="nodes must be non-empty (mirror the step-0 task-chunk "
                                       "fallback client-side)")
        with lock:
            sg, _forward_ms = v1graph.score_tools(
                bundle, req.nodes, req.task_emb, req.sys_emb, [t.emb for t in req.tools])
        return {"names": [t.name for t in req.tools], "scores_q": [_q(s) for s in sg],
                "tokens": [int(t.tokens) for t in req.tools],
                "checkpoint_id": bundle.checkpoint_id}

    @app.post("/v1/score/trace")
    def score_trace(req: ScoreTraceBody, request: Request):
        _auth(request)
        if isinstance(req, ScoreTraceV1Request):     # contract-field dispatch
            return _score_trace_v1(req)
        with lock:
            parsed = scorer.parse(req.messages)
            server_sum = chunk_checksum(parsed.chunks)
            if server_sum != req.chunk_checksum:
                # cross-language chunker-parity guard, enforced live: scoring a divergent parse
                # would land scores on the wrong rows. The client fails open on 409.
                raise HTTPException(status_code=409, detail={
                    "error": "chunk_checksum mismatch",
                    "server_checksum": server_sum,
                    "server_chunks": len(parsed.chunks),
                    "server_cur_step": parsed.cur_step,
                })
            n = len(parsed.chunks)
            if any(g < 0 or g >= n for g in req.live_gi):
                raise HTTPException(status_code=422,
                                    detail=f"live_gi out of range for {n} parsed chunks")
            if any(j < 0 or j >= len(req.live_gi) for j in req.mask):
                raise HTTPException(status_code=422,
                                    detail=f"mask out of range for {len(req.live_gi)} live rows")
            if not req.live_gi or not req.mask:          # nothing to decide: all rows never-cut
                return {"scores_q": [GRID] * len(req.live_gi), "tau_q": bundle.tau_q,
                        "checkpoint_id": bundle.checkpoint_id,
                        "timings_ms": {"embed": 0.0, "forward": 0.0}}
            sc, tau, timings = scorer.score_trace(parsed, req.live_gi, req.mask,
                                                  conv_id=req.conv_id)
            return {"scores_q": [_q(s) for s in sc], "tau_q": _q(tau),
                    "checkpoint_id": bundle.checkpoint_id, "timings_ms": timings}

    @app.post("/v1/score/tools")
    def score_tools(req: ScoreToolsBody, request: Request):
        _auth(request)
        if isinstance(req, ScoreToolsV1Request):     # contract-field dispatch
            return _score_tools_v1(req)
        with lock:
            sg, toks, names = scorer.score_request_tools(req.messages, req.tools)
        if names is None:                                # ineligible/failed: fail-open shape
            return {"names": [], "scores_q": [], "tokens": [],
                    "checkpoint_id": bundle.checkpoint_id}
        return {"names": names, "scores_q": [_q(s) for s in sg],
                "tokens": [int(t) for t in toks], "checkpoint_id": bundle.checkpoint_id}

    @app.post("/v1/score/rules")
    def score_rules(req: ScoreRulesRequest, request: Request):
        """Rule head — NOT wired into any proxy consumer yet; tau_hint_q is advisory only."""
        _auth(request)
        cands = req.rules if req.rules is not None else rule_defaults
        if any(not (isinstance(r, dict) and r.get("eid") and r.get("text")) for r in cands):
            raise HTTPException(status_code=422, detail="rules items must carry eid and text")
        with lock:
            scores = scorer.score_request_rules(req.messages, req.tools, cands, req.step)
        return {"scores_q": {eid: _q(s) for eid, s in scores.items()},
                "tau_hint_q": RULE_TAU_HINT_Q, "checkpoint_id": bundle.checkpoint_id,
                "description": RULE_TAU_DESCRIPTION}

    @app.post("/v1/score/gate")
    def score_gate(req: ScoreGateRequest, request: Request):
        """Gate/brief head — no proxy consumer yet. fire = score_q >= tau_q (0.5, locked)."""
        _auth(request)
        with lock:
            s = scorer.score_request_gate(req.messages, req.tools, req.brief)
        if s is None:                                    # ineligible/failed/empty brief:
            return {"score_q": GRID, "tau_q": GATE_TAU_Q, "fire": True,   # fail open = serve
                    "fail_open": True, "checkpoint_id": bundle.checkpoint_id}
        sq = _q(s)
        return {"score_q": sq, "tau_q": GATE_TAU_Q, "fire": sq >= GATE_TAU_Q,
                "fail_open": False, "checkpoint_id": bundle.checkpoint_id}

    return app
