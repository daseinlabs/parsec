"""FastAPI surface for BOTH brain contracts, dispatched on the request's `contract` field:

  brain-api-dev/v0  (contracts/schemas/brain-api-dev.schema.json)
    ⚠️ Dev-raw posture: carries the internal message view (RAW TEXT) — dev/dogfood machines
    only, behind the client's explicit PARSEC_BRAIN_DEV_RAW=1 opt-in.
  brain-api/v1      (contracts/schemas/brain-api.schema.json)
    Data-plane clean: client-computed vectors + structural features + opaque ids in; raw
    text is UNREPRESENTABLE (every string field is an enum, a hex id, or a bounded
    identifier). Requires the request checkpoint_id to match the loaded bundle (409
    otherwise — the §8.2 matched-pair guard: features and weights must move together).

The bundle loads (and self-validates) at app construction: a bad checkpoint refuses to start.

Run: uvicorn --factory parsec_brain.app:create_app --port 8080
"""
from __future__ import annotations

from . import _flags  # noqa: F401  parity pins before bundle/scorer (vendored) imports

import gzip
import os
import statistics
import threading
import time
from typing import Annotated, Literal, Union

import numpy as np
from fastapi import FastAPI, HTTPException, Request
from pydantic import BaseModel, ConfigDict, Field

from . import keyauth, v1graph
from ._log import COUNTERS, conv_sha8, count_fail_open, get_logger
from .bundle import load_bundle
from .scorer import _EMBED_DIM, TraceScorer, chunk_checksum

log = get_logger("app")


class _GunzipRequests:
    """ASGI middleware: transparently decompress `Content-Encoding: gzip`
    request bodies BEFORE routing/parsing (perf research 2026-09-02 §3 — the
    v2 trace payload is MB-scale, ~5-10x-compressible JSON). The client only
    gzips after /v1/bundle advertises `accept_gzip`, so old clients ride the
    identity wire untouched. Pure ASGI (not BaseHTTPMiddleware) so the
    replaced receive stream is what routing actually reads."""

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope["type"] == "http":
            hdrs = {k.lower(): v for k, v in (scope.get("headers") or [])}
            if hdrs.get(b"content-encoding", b"").lower() == b"gzip":
                body = b""
                while True:
                    msg = await receive()
                    body += msg.get("body", b"")
                    if not msg.get("more_body", False):
                        break
                try:
                    data = gzip.decompress(body)
                except OSError:
                    await send({"type": "http.response.start", "status": 400,
                                "headers": [(b"content-type", b"application/json")]})
                    await send({"type": "http.response.body",
                                "body": b'{"detail":"invalid gzip request body"}'})
                    return
                scope = dict(scope)
                scope["headers"] = [
                    (k, v) for (k, v) in scope["headers"]
                    if k.lower() not in (b"content-encoding", b"content-length")
                ] + [(b"content-length", str(len(data)).encode())]
                sent = False

                async def replay():
                    nonlocal sent
                    if sent:
                        return {"type": "http.disconnect"}
                    sent = True
                    return {"type": "http.request", "body": data, "more_body": False}

                await self.app(scope, replay, send)
                return
        await self.app(scope, receive, send)

CONTRACT = "brain-api-dev/v0"
CONTRACT_V1 = "brain-api/v1"
CONTRACT_V2 = "brain-api/v2"
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


# doom-head run-state input: loop_feats = [cur loop frac, trailing-3 mean, slope over 3,
# trailing-6 frac >= 0.34] — 4 bounded numbers, client-computed (commands are raw text; the
# pure function ports, same parity posture as the chunker guard). No text representable.
Gf4 = Annotated[list[Annotated[float, Field(ge=-1.0, le=1.0)]],
                Field(min_length=4, max_length=4)]


class ScoreTraceRequest(BaseModel):
    contract: Literal["brain-api-dev/v0"]
    conv_id: str = Field(max_length=128)
    messages: list[dict]
    live_gi: list[int]
    mask: list[int]
    cur_step: int
    chunk_checksum: str
    target_cov: str | None = None    # accepted per schema; tau resolves at bundle load, not here
    gf: Gf4 | None = None            # omitted = doom not scored (v0 behavior)


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
    gf: Gf4 | None = None                        # loop_feats 4-vector; omitted = no doom score


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


# ---- brain-api/v2: TEXT + features in, the server embeds ------------------------------------
# v2 is v1 with exactly three substitutions (emb_text/emb_cmd/emb_head -> text/cmd/head,
# task_emb -> task_text, sys_emb -> sys_text, tool.emb -> tool.schema_text). Raw text IS
# representable here — deliberately; the embedder moved server-side (DIRECTION.md §5 rev.
# 2026-07-20, docs/server-side-embedding.md). v1 stays SERVED for deployments that require
# text never to leave the machine. The checkpoint_id matched-pair guard is retained in full:
# that is why v2 derives from v1 and not from the guardless brain-api-dev/v0.
#
# Every v2 request is adapted to its v1 twin in-process (_v2_*_to_v1 below) so there is exactly
# ONE scoring path — parity is by construction, not by a second implementation kept in step.
Text2000 = Annotated[str, Field(max_length=2000)]
Text240 = Annotated[str, Field(max_length=240)]


class V2Node(BaseModel):
    """One live chunk carrying text instead of vectors. Field-for-field the v1 node otherwise.

    EMPTY-STRING SEMANTICS ARE LOAD-BEARING: an empty cmd/head must become a ZERO VECTOR, not
    the embedding of "" — v1's client mirrored this (featurize::vec_or_zeros never called the
    embedder on the empty string) and the cross-contract parity gate pins it."""
    model_config = ConfigDict(extra="forbid")
    text: Text2000                   # chunk text, CLIENT-truncated to 2000 chars
    cmd: str                         # producing command, UNTRUNCATED (v1 embedded it whole)
    head: Text240                    # observation head, CLIENT-truncated to 240 chars
    struct: list[float] = Field(min_length=21, max_length=21)   # node_struct_with_type row
    step: int = Field(ge=0)
    kind: Literal["read", "grep", "other", "reasoning", "asst"]
    tokens: int = Field(ge=1)
    file_id: str | None = Field(default=None, pattern=_HEX16)
    lo: int | None = None
    hi: int | None = None
    cmd_id: str | None = Field(default=None, pattern=_HEX16)
    head_id: str | None = Field(default=None, pattern=_HEX16)


class ScoreTraceV2Request(BaseModel):
    model_config = ConfigDict(extra="forbid")
    contract: Literal["brain-api/v2"]
    conv_id: str = Field(pattern=_IDENT)
    checkpoint_id: str = Field(pattern=_HEX64)
    cur_step: int = Field(ge=0)
    nodes: list[V2Node]
    task_text: Text2000
    sys_text: Text2000 | None = None
    mask: list[int]
    # 49-col rows as on v1, EXCEPT the trailing dupcos pair (cols 47-48) which the client
    # cannot compute without an embedder. The client sends them zeroed and the server fills
    # them from its own content embeddings — see _v2_fill_dupcos.
    decided_struct: list[list[float]]
    edges_supersession: list[tuple[int, int]]
    target_cov: str | None = Field(default=None, pattern=r"^0\.\d{2}$")
    gf: Gf4 | None = None


class V2Tool(BaseModel):
    model_config = ConfigDict(extra="forbid")
    name: str = Field(pattern=_IDENT)
    schema_text: str                             # serialized schema; the server embeds it
    tokens: int = Field(ge=1)


class ScoreToolsV2Request(BaseModel):
    model_config = ConfigDict(extra="forbid")
    contract: Literal["brain-api/v2"]
    conv_id: str = Field(pattern=_IDENT)
    checkpoint_id: str = Field(pattern=_HEX64)
    nodes: list[V2Node]
    task_text: Text2000
    sys_text: Text2000 | None = None
    tools: list[V2Tool]


ScoreTraceBody = Annotated[Union[ScoreTraceRequest, ScoreTraceV1Request, ScoreTraceV2Request],
                           Field(discriminator="contract")]
ScoreToolsBody = Annotated[Union[ScoreToolsRequest, ScoreToolsV1Request, ScoreToolsV2Request],
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


class ScoreRulesV1Request(BaseModel):
    """Rule head on the v1 contract: the trace payload's graph fields + the fire step. The
    rule roster AND rule text live server-side (rules.json) — nothing text-shaped rides in.
    `nodes` is the tool-spec chunk pipeline view (the same featurization the v1 tools request
    carries); mask/decided_struct are decision-path fields the rule head never reads and are
    deliberately not part of this request."""
    model_config = ConfigDict(extra="forbid")
    contract: Literal["brain-api/v1"]
    conv_id: str = Field(pattern=_IDENT)
    checkpoint_id: str = Field(pattern=_HEX64)
    cur_step: int = Field(ge=0)                  # the fire step (nearest-earlier clamp applies)
    nodes: list[V1Node]
    task_emb: Vec1024
    sys_emb: Vec1024 | None = None
    edges_supersession: list[tuple[int, int]]    # required even when empty, like score/trace


class ScoreRulesV2Request(BaseModel):
    """Rule head on v2: the v1 rule request with text in the node slots. The rule roster and
    rule text remain SERVER-owned (rules.json) and server-embedded — unchanged from v1."""
    model_config = ConfigDict(extra="forbid")
    contract: Literal["brain-api/v2"]
    conv_id: str = Field(pattern=_IDENT)
    checkpoint_id: str = Field(pattern=_HEX64)
    cur_step: int = Field(ge=0)
    nodes: list[V2Node]
    task_text: Text2000
    sys_text: Text2000 | None = None
    edges_supersession: list[tuple[int, int]]


class NeighborsRequest(BaseModel):
    contract: Literal["brain-api-dev/v0"]
    conv_id: str = Field(max_length=128)
    task_text: str                               # first user message internal text[:2000]


class NeighborsV1Request(BaseModel):
    model_config = ConfigDict(extra="forbid")
    contract: Literal["brain-api/v1"]
    conv_id: str = Field(pattern=_IDENT)
    checkpoint_id: str = Field(pattern=_HEX64)
    task_vec: Vec1024                            # client-embedded task head; raw text never rides


class NeighborsV2Request(BaseModel):
    model_config = ConfigDict(extra="forbid")
    contract: Literal["brain-api/v2"]
    conv_id: str = Field(pattern=_IDENT)
    checkpoint_id: str = Field(pattern=_HEX64)
    task_text: Text2000                          # server-embedded; "" -> zero vector


ScoreRulesBody = Annotated[Union[ScoreRulesRequest, ScoreRulesV1Request, ScoreRulesV2Request],
                           Field(discriminator="contract")]
NeighborsBody = Annotated[Union[NeighborsRequest, NeighborsV1Request, NeighborsV2Request],
                          Field(discriminator="contract")]


def create_app() -> FastAPI:
    bundle = load_bundle()           # self-validating: any mismatch raises, the app never starts
    scorer = TraceScorer(bundle)
    # PARSEC_TORCH_THREADS: pin torch's intra-op pool to the Cloud Run CPU
    # quota — under cgroups torch reads the HOST core count and oversubscribes
    # (docs/perf-research-2026-09-02.md §2.4). Unset = torch defaults.
    tn = os.environ.get("PARSEC_TORCH_THREADS", "").strip()
    if tn.isdigit() and int(tn) > 0:
        import torch
        torch.set_num_threads(int(tn))
        log.info("torch.set_num_threads(%s)", tn)
    # Warm the lazy encoder at STARTUP: bge-large used to load inside the
    # first scoring request of every new instance — several-to-tens of
    # seconds under the request lock (perf research §2.5). Startup cost now
    # lands in the Cloud Run startup-probe window. Fail-open: a warm failure
    # only defers the load back to the first request, as before.
    try:
        scorer._embed(["parsec embedder warm-up"])
    except Exception as e:  # noqa: BLE001 — warm-up must never block startup
        log.warning("embedder warm-up failed (%s) — deferred to first request",
                    type(e).__name__)
    lock = threading.Lock()          # one CPU forward at a time; scorer caches are shared state
    key = os.environ.get("PARSEC_BRAIN_KEY", "")
    # Per-user entitlement gate: when a platform URL is configured, the bearer
    # token is the caller's psc_ key, validated against the platform's
    # /keys/validate seam (DIRECTION §7). Otherwise fall back to the static
    # shared-key gate below. Platform mode takes precedence when both are set.
    platform_url = os.environ.get("PARSEC_PLATFORM_URL", "").strip()
    # rules.json subset served when a /v1/score/rules request omits `rules`: the governor's live
    # roster = status active|always_on (candidate/retired rules are never fired unrequested).
    rule_defaults = [{"eid": r["eid"], "text": r["text"]} for r in bundle.rules
                     if r.get("status") in _RULE_DEFAULT_STATUS]
    app = FastAPI(title="parsec-brain", version="0.1.0")
    app.add_middleware(_GunzipRequests)

    def _bearer(request: Request) -> str | None:
        h = request.headers.get("authorization", "")
        return h[7:] if h[:7].lower() == "bearer " else None

    def _auth(request: Request) -> None:
        if platform_url:
            if not keyauth.check(platform_url, _bearer(request)):
                raise HTTPException(status_code=401, detail="key invalid or not entitled")
        elif key and request.headers.get("authorization") != f"Bearer {key}":
            raise HTTPException(status_code=401, detail="missing or invalid bearer token")

    @app.middleware("http")
    async def _count_and_time(request: Request, call_next):
        """Observability only: request counter (served on /health) + a DEBUG access line.
        The per-endpoint INFO lines below carry the scoring detail; nothing here reads or
        logs bodies (data-plane rule)."""
        if request.url.path == "/health":
            return await call_next(request)
        COUNTERS["requests"] += 1
        t0 = time.perf_counter()
        response = await call_next(request)
        log.debug("access path=%s status=%d total_ms=%.1f", request.url.path,
                  response.status_code, (time.perf_counter() - t0) * 1000.0)
        return response

    def _score_stats(scores_q: list[int]) -> str:
        if not scores_q:
            return "n=0"
        return f"n={len(scores_q)} min_q={min(scores_q)} max_q={max(scores_q)}"

    @app.get("/health")
    def health():
        # fail_opens/requests: process-wide counters (CLAUDE.md: fail-open counted, alertable)
        return {"status": "ok", "fail_opens": COUNTERS["fail_opens"],
                "requests": COUNTERS["requests"]}

    @app.get("/v1/bundle")
    def bundle_info(request: Request):
        _auth(request)
        info = {
            "contract": CONTRACT,
            "contracts": [CONTRACT, CONTRACT_V1, CONTRACT_V2],   # all endpoints dispatch on all
            "checkpoint_id": bundle.checkpoint_id,
            "tau_q": bundle.tau_q,
            "target_cov": bundle.target_cov,
            "grid": GRID,
            "heads": ["curator", "tool", "rule", "gate"],   # rule/gate: no proxy consumer yet
            # request-compression capability (perf research §3): clients gzip
            # trace bodies only after seeing this — version-skew-safe.
            "accept_gzip": True,
            # neighbors: True when the hoods artifact is mounted (PARSEC_HOODS_PKL); False =
            # nf=None, +3 zero block-parity cols (valid: trained with 20% block dropout)
            "neighbors": bundle.hoods is not None,
            # doom head provenance: gf width the ckpt expects; scored only when a trace
            # request carries `gf` (no consumer sends it by default)
            "doom": {"gf": bundle.doom_gf, "served": bundle.doom_gf > 0},
            "flags": bundle.flags,
        }
        if bundle.hoods is not None:
            info["hoods_anchors"] = len(bundle.hoods.task_embs)
        return info

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
            # (no forward runs here, so no doom_q even when gf rode in — nothing was scored)
            return {"scores_q": [GRID] * n, "tau_q": bundle.tau_q,
                    "checkpoint_id": bundle.checkpoint_id,
                    "timings_ms": {"embed": 0.0, "forward": 0.0}}
        with lock:
            sc, tau, timings, doom = v1graph.score_trace(
                bundle, req.nodes, req.task_emb, req.sys_emb, req.mask,
                req.decided_struct, req.edges_supersession, gf=req.gf)
        out = {"scores_q": [_q(s) for s in sc], "tau_q": _q(tau),
               "checkpoint_id": bundle.checkpoint_id, "timings_ms": timings}
        if doom is not None:
            out["doom_q"] = _q(doom)
        log.info("score/trace contract=v1 conv=%s n_nodes=%d n_mask=%d heads=curator%s "
                 "graph_ms=%s forward_ms=%s tau_q=%d %s%s neighbors=%s status=200",
                 conv_sha8(req.conv_id), n, len(req.mask),
                 "+doom" if doom is not None else "",
                 timings.get("embed"), timings.get("forward"), out["tau_q"],
                 _score_stats(out["scores_q"]),
                 f" doom_q={out['doom_q']}" if doom is not None else "",
                 bundle.hoods is not None)
        return out

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

    # ---- brain-api/v2 -> v1 adaptation -------------------------------------------------------
    # Every v2 request is embedded here and handed to the v1 handler above. One scoring path,
    # so v1/v2 parity is structural rather than two implementations kept in step. The cost is
    # two lock acquisitions per v2 request (embed, then score) — safe because scoring is a pure
    # function of the request and the scorer caches are performance only, never serving state.

    def _v2_embed(texts: list[str]) -> list[list[float]]:
        """Embed with v1's empty-string semantics: "" -> zero vector, NEVER embedded.

        The v1 client mirrored this in featurize::vec_or_zeros; embedding "" instead would
        shift every zero slot off the trained convention. Caller must hold `lock` —
        scorer.cache is shared mutable state."""
        nonempty = [t for t in texts if t]
        if nonempty:
            scorer._embed(nonempty)              # populates scorer.cache
        zed = [0.0] * _EMBED_DIM
        return [list(scorer.cache[t]) if t else zed for t in texts]

    def _v2_embed_raw(texts: list[str]) -> list:
        """_v2_embed without the per-element list() copies: rows are the
        cached vectors VERBATIM (np or list). Only for consumers that go
        straight to np.asarray / model_construct — never for values that
        enter a validated pydantic field. Caller must hold `lock`."""
        nonempty = [t for t in texts if t]
        if nonempty:
            scorer._embed(nonempty)
        zed = [0.0] * _EMBED_DIM
        return [scorer.cache[t] if t else zed for t in texts]

    def _v2_nodes_to_v1(nodes: list[V2Node]) -> tuple[list[V1Node], list[list[float]]]:
        """(v1 nodes, content embeddings aligned to node rows). ONE embedder batch over
        text+cmd+head, mirroring the v1 client's single batch. Caller must hold `lock`."""
        n = len(nodes)
        v = _v2_embed_raw([x.text for x in nodes] + [x.cmd for x in nodes]
                          + [x.head for x in nodes])
        vt, vc, vh = v[:n], v[n:2 * n], v[2 * n:]
        # model_construct: the fields were already validated at the wire as
        # V2Node; re-validating 3x1024 floats per node through V1Node cost
        # 0.4-1.0s/request at n~2000 (perf research 2026-09-02 §2.3). The
        # wrapping ScoreTraceV1Request accepts the instances unrevalidated
        # (pydantic v2 revalidate_instances='never').
        out = [V1Node.model_construct(emb_text=vt[i], emb_cmd=vc[i], emb_head=vh[i],
                                      struct=x.struct, step=x.step, kind=x.kind,
                                      tokens=x.tokens, file_id=x.file_id, lo=x.lo,
                                      hi=x.hi, cmd_id=x.cmd_id, head_id=x.head_id)
               for i, x in enumerate(nodes)]
        return out, vt

    def _v2_fill_dupcos(content: list[list[float]], steps: list[int], mask: list[int],
                        decided_struct: list[list[float]]) -> list[list[float]]:
        """Fill the trailing AC_DUPCOS pair (cols 47-48 of 49) from server-side embeddings.

        A v1 client computes these locally from its own embedder; a v2 client has none, so it
        sends them zeroed and we recompute. This is the ONLY readout column the client cannot
        produce without an embedder — everything else is structural. Byte-identical to the dev
        path's TraceScorer._het_readout AC_DUPCOS block: same 1e-8 normalization epsilon, same
        first-max-on-ties argmax, same 1/(1+dstep). Indexed from the END so it tracks
        read_struct rather than assuming 49."""
        emb = np.asarray(content, np.float32)
        en = emb / (np.linalg.norm(emb, axis=1, keepdims=True) + 1e-8)
        stp = np.asarray(steps)
        out = [list(r) for r in decided_struct]
        for k, j in enumerate(mask):
            ej = np.where(stp < stp[j])[0]
            if len(ej):
                sims = en[ej] @ en[j]
                m = int(np.argmax(sims))
                out[k][-2] = float(sims[m])
                out[k][-1] = float(1.0 / (1.0 + stp[j] - stp[ej[m]]))
            else:
                out[k][-2] = 0.0
                out[k][-1] = 0.0
        return out

    def _score_trace_v2(req: ScoreTraceV2Request):
        _v1_checkpoint_guard(req.checkpoint_id)
        # These three checks duplicate _score_trace_v1's, deliberately: the dupcos fill indexes
        # by mask and writes by row, so a malformed request must 422 here rather than
        # IndexError into a 500 further down.
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
        with lock:
            v1nodes, content = _v2_nodes_to_v1(req.nodes)
            task_emb = _v2_embed([req.task_text])[0]
            sys_emb = _v2_embed([req.sys_text])[0] if req.sys_text else None
            ds = (_v2_fill_dupcos(content, [x.step for x in req.nodes], req.mask,
                                  req.decided_struct)
                  if req.nodes and req.mask else req.decided_struct)
        return _score_trace_v1(ScoreTraceV1Request(
            contract="brain-api/v1", conv_id=req.conv_id, checkpoint_id=req.checkpoint_id,
            cur_step=req.cur_step, nodes=v1nodes, task_emb=task_emb, sys_emb=sys_emb,
            mask=req.mask, decided_struct=ds, edges_supersession=req.edges_supersession,
            target_cov=req.target_cov, gf=req.gf))

    def _score_tools_v2(req: ScoreToolsV2Request):
        _v1_checkpoint_guard(req.checkpoint_id)
        with lock:
            v1nodes, _ = _v2_nodes_to_v1(req.nodes)
            task_emb = _v2_embed([req.task_text])[0]
            sys_emb = _v2_embed([req.sys_text])[0] if req.sys_text else None
            tool_embs = _v2_embed([t.schema_text for t in req.tools])
        return _score_tools_v1(ScoreToolsV1Request(
            contract="brain-api/v1", conv_id=req.conv_id, checkpoint_id=req.checkpoint_id,
            nodes=v1nodes, task_emb=task_emb, sys_emb=sys_emb,
            tools=[V1Tool.model_construct(name=t.name, emb=tool_embs[i], tokens=t.tokens)
                   for i, t in enumerate(req.tools)]))

    def _score_rules_v2(req: ScoreRulesV2Request):
        _v1_checkpoint_guard(req.checkpoint_id)
        with lock:
            v1nodes, _ = _v2_nodes_to_v1(req.nodes)
            task_emb = _v2_embed([req.task_text])[0]
            sys_emb = _v2_embed([req.sys_text])[0] if req.sys_text else None
        return _score_rules_v1(ScoreRulesV1Request(
            contract="brain-api/v1", conv_id=req.conv_id, checkpoint_id=req.checkpoint_id,
            cur_step=req.cur_step, nodes=v1nodes, task_emb=task_emb, sys_emb=sys_emb,
            edges_supersession=req.edges_supersession))

    @app.post("/v1/score/trace")
    def score_trace(req: ScoreTraceBody, request: Request):
        _auth(request)
        if isinstance(req, ScoreTraceV2Request):     # contract-field dispatch
            return _score_trace_v2(req)
        if isinstance(req, ScoreTraceV1Request):
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
                # (no forward runs, so no doom_q even when gf rode in — nothing was scored)
                return {"scores_q": [GRID] * len(req.live_gi), "tau_q": bundle.tau_q,
                        "checkpoint_id": bundle.checkpoint_id,
                        "timings_ms": {"embed": 0.0, "forward": 0.0}}
            sc, tau, timings = scorer.score_trace(parsed, req.live_gi, req.mask,
                                                  conv_id=req.conv_id, gf=req.gf)
            doom = scorer.last_doom                      # set by the same forward, under lock
            nbr = scorer.last_nbr_blocks
            out = {"scores_q": [_q(s) for s in sc], "tau_q": _q(tau),
                   "checkpoint_id": bundle.checkpoint_id, "timings_ms": timings}
            if doom is not None:
                out["doom_q"] = _q(doom)
            log.info("score/trace contract=dev conv=%s n_msgs=%d n_chunks=%d n_live=%d "
                     "n_mask=%d heads=curator%s embed_ms=%s forward_ms=%s "
                     "embed_cache=%d/%d/%d sz=%d tau_q=%d %s%s "
                     "neighbors=%s status=200",
                     conv_sha8(req.conv_id), len(req.messages), len(parsed.chunks),
                     len(req.live_gi), len(req.mask), "+doom" if doom is not None else "",
                     timings.get("embed"), timings.get("forward"),
                     scorer.cache.hits, scorer.cache.misses, scorer.cache.evicted,
                     len(scorer.cache), out["tau_q"],
                     _score_stats(out["scores_q"]),
                     f" doom_q={out['doom_q']}" if doom is not None else "",
                     nbr if nbr is not None else False)
            return out

    @app.post("/v1/score/tools")
    def score_tools(req: ScoreToolsBody, request: Request):
        _auth(request)
        if isinstance(req, (ScoreToolsV1Request, ScoreToolsV2Request)):  # contract dispatch
            v2 = isinstance(req, ScoreToolsV2Request)
            out = _score_tools_v2(req) if v2 else _score_tools_v1(req)
            log.info("score/tools contract=%s conv=%s n_nodes=%d n_tools=%d %s status=200",
                     "v2" if v2 else "v1",
                     conv_sha8(req.conv_id), len(req.nodes), len(req.tools),
                     _score_stats(out["scores_q"]))
            return out
        with lock:
            sg, toks, names = scorer.score_request_tools(req.messages, req.tools)
        if names is None:                                # ineligible/failed: fail-open shape
            log.info("score/tools contract=dev conv=%s n_msgs=%d n_tools=%d fail_open_shape=1 "
                     "status=200", conv_sha8(req.conv_id), len(req.messages), len(req.tools))
            return {"names": [], "scores_q": [], "tokens": [],
                    "checkpoint_id": bundle.checkpoint_id}
        out = {"names": names, "scores_q": [_q(s) for s in sg],
               "tokens": [int(t) for t in toks], "checkpoint_id": bundle.checkpoint_id}
        log.info("score/tools contract=dev conv=%s n_msgs=%d n_tools=%d %s status=200",
                 conv_sha8(req.conv_id), len(req.messages), len(names),
                 _score_stats(out["scores_q"]))
        return out

    def _rules_response(scores_q: dict[str, int], cands: list[dict], fire_step: int) -> dict:
        """Both contracts' rules response: the existing fields plus the per-rule roster view
        [{eid, text, p_q, fire_step}] for the CURRENT step. Rule text is server-owned
        (rules.json) — returning it is data-plane-clean; fire_step echoes the requested step
        (the governor's dedupe key). Fail-open keeps rules=[] alongside scores_q={}."""
        by_eid = {r["eid"]: r for r in cands}
        return {"scores_q": scores_q,
                "rules": [{"eid": eid, "text": by_eid[eid]["text"], "p_q": q,
                           "fire_step": int(fire_step)}
                          for eid, q in scores_q.items() if eid in by_eid],
                "tau_hint_q": RULE_TAU_HINT_Q, "checkpoint_id": bundle.checkpoint_id,
                "description": RULE_TAU_DESCRIPTION}

    def _score_rules_v1(req: ScoreRulesV1Request):
        _v1_checkpoint_guard(req.checkpoint_id)
        n = len(req.nodes)
        if not req.nodes:
            raise HTTPException(status_code=422,
                                detail="nodes must be non-empty (mirror the step-0 task-chunk "
                                       "fallback client-side)")
        if any(not (0 <= a < n and 0 <= b < n) for (a, b) in req.edges_supersession):
            raise HTTPException(status_code=422,
                                detail=f"edges_supersession out of range for {n} nodes")
        cands = rule_defaults                        # roster + text live server-side on v1
        with lock:
            try:
                # rule texts are SERVER data: embed them with the scorer's backend, exactly
                # as the dev path does (untruncated — assemble's exact cache key).
                scorer._embed([r["text"] for r in cands])
                rule_embs = [scorer.cache[r["text"]] for r in cands]
                sc, forward_ms = v1graph.score_rules(
                    bundle, req.nodes, req.task_emb, req.sys_emb, req.edges_supersession,
                    req.cur_step, rule_embs)
                scores = {cands[k]["eid"]: float(sc[k]) for k in range(len(sc))}
            except Exception as e:
                log.debug("v1 rule scoring exception detail", exc_info=True)
                count_fail_open(log, f"v1 rule scoring failed ({type(e).__name__}) -> fail-open")
                scores, forward_ms = {}, 0.0
        out = _rules_response({eid: _q(s) for eid, s in scores.items()}, cands, req.cur_step)
        log.info("score/rules contract=v1 conv=%s n_nodes=%d n_rules=%d fire_step=%d "
                 "forward_ms=%s status=200", conv_sha8(req.conv_id), n, len(out["rules"]),
                 req.cur_step, forward_ms)
        return out

    @app.post("/v1/score/rules")
    def score_rules(req: ScoreRulesBody, request: Request):
        """Rule head — tau_hint_q is advisory only (bench calibration owns the firing tau)."""
        _auth(request)
        if isinstance(req, ScoreRulesV2Request):     # contract-field dispatch
            return _score_rules_v2(req)
        if isinstance(req, ScoreRulesV1Request):
            return _score_rules_v1(req)
        cands = req.rules if req.rules is not None else rule_defaults
        if any(not (isinstance(r, dict) and r.get("eid") and r.get("text")) for r in cands):
            raise HTTPException(status_code=422, detail="rules items must carry eid and text")
        with lock:
            scores = scorer.score_request_rules(req.messages, req.tools, cands, req.step)
        out = _rules_response({eid: _q(s) for eid, s in scores.items()}, cands, req.step)
        log.info("score/rules contract=dev conv=%s n_msgs=%d n_rules=%d fire_step=%d "
                 "fail_open_shape=%d status=200", conv_sha8(req.conv_id), len(req.messages),
                 len(out["rules"]), req.step, int(not scores))
        return out

    @app.post("/v1/neighbors")
    def neighbors(req: NeighborsBody, request: Request):
        """Cross-trace neighbor cost baseline — called ONCE per conversation by the proxy
        (the neighbor set is per-task constant; the client caches the result). null median =
        hoods off / <4 neighbor costs (the runaway signal stays inert, reference semantics)."""
        _auth(request)
        contract = ("v1" if isinstance(req, NeighborsV1Request)
                    else "v2" if isinstance(req, NeighborsV2Request) else "dev")
        if isinstance(req, (NeighborsV1Request, NeighborsV2Request)):
            _v1_checkpoint_guard(req.checkpoint_id)
        costs: list[float] = []
        if bundle.hoods is not None:
            if isinstance(req, NeighborsV1Request):
                task_vec = np.asarray(req.task_vec, dtype=np.float32)
            elif req.task_text:
                with lock:                           # dev: embed via the request's embed backend
                    task_vec = np.asarray(scorer._embed([req.task_text[:2000]])[0],
                                          dtype=np.float32)
            else:
                task_vec = np.zeros(1024, dtype=np.float32)
            costs = bundle.hoods.neighbor_costs(task_vec)
        median = float(statistics.median(costs)) if len(costs) >= 4 else None
        log.info("neighbors contract=%s conv=%s active=%s nbr_count=%d median=%s status=200",
                 contract, conv_sha8(req.conv_id), bundle.hoods is not None, len(costs),
                 "null" if median is None else round(median, 1))
        return {"nbr_cost_median": median, "nbr_count": len(costs),
                "neighbors_active": bundle.hoods is not None,
                "checkpoint_id": bundle.checkpoint_id}

    @app.post("/v1/score/gate")
    def score_gate(req: ScoreGateRequest, request: Request):
        """Gate/brief head — no proxy consumer yet. fire = score_q >= tau_q (0.5, locked)."""
        _auth(request)
        with lock:
            s = scorer.score_request_gate(req.messages, req.tools, req.brief)
        if s is None:                                    # ineligible/failed/empty brief:
            log.info("score/gate contract=dev conv=%s n_msgs=%d fail_open=1 status=200",
                     conv_sha8(req.conv_id), len(req.messages))
            return {"score_q": GRID, "tau_q": GATE_TAU_Q, "fire": True,   # fail open = serve
                    "fail_open": True, "checkpoint_id": bundle.checkpoint_id}
        sq = _q(s)
        log.info("score/gate contract=dev conv=%s n_msgs=%d score_q=%d fire=%s status=200",
                 conv_sha8(req.conv_id), len(req.messages), sq, sq >= GATE_TAU_Q)
        return {"score_q": sq, "tau_q": GATE_TAU_Q, "fire": sq >= GATE_TAU_Q,
                "fail_open": False, "checkpoint_id": bundle.checkpoint_id}

    return app
