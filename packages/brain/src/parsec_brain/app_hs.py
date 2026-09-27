"""HS brain app: serves the hidden-states curator on brain-api/v3 (+ a brain-api/v2 adapter).

Selected by create_app() when PARSEC_HS_CKPT is set; the legacy bge brain is untouched otherwise.
One process holds one checkpoint, so this app answers only the HS bundle's checkpoint_id.

Routes (contracts/schemas/brain-api-v3.schema.json is the source of truth):
  GET  /health
  GET  /v1/bundle        v3 handshake: contracts, widths, spec_version, per-kind taus, encoder
  POST /v1/score/trace   brain-api/v3 (native) | brain-api/v2 (adapted, see below)
  POST /v1/score/tools   brain-api/v2 body (the v3 proxy keeps sending v2 bodies here)
  POST /v1/score/rules   brain-api/v2 body
  POST /v1/neighbors     brain-api/v2 body; HS trained with AC_NEIGHBORS=0 -> always inactive

brain-api/v2 score/trace on the HS brain (old plugins pointed at this service): nodes are adapted
with scorer_hs.adapt_v2_nodes (rereq from the live set, cut cols 0, ext "none"); the 49-col decided
row becomes 104 (drop changeprone col 42, rereq 16-21 from the adapted node, zeros elsewhere for the
brain-filled blocks); the reply folds the per-kind taus into the scores so a single-tau client cuts
exactly where the per-kind rule would (ignoring fought, which v2 cannot express):
  q' = clamp(q - tau_q[kind] + T, 0, GRID) with T = tau_q["read"] returned as tau_q; asst -> GRID.
"""
from __future__ import annotations

import os
import threading
import time
from typing import Annotated, Literal, Union

from fastapi import FastAPI, HTTPException, Request
from pydantic import BaseModel, ConfigDict, Field

from . import _flags_hs, keyauth
from ._log import COUNTERS, conv_sha8, count_fail_open, get_logger
from .app import (_HEX16, _HEX64, _IDENT, GRID, RULE_TAU_DESCRIPTION, RULE_TAU_HINT_Q,
                  _RULE_DEFAULT_STATUS, CONTRACT_V2, Gf4, NeighborsV2Request,
                  ScoreRulesV2Request, ScoreToolsV2Request, ScoreTraceV2Request, Text240, Text2000,
                  _GunzipRequests, _q)
from .bundle import _PKG_MODELS, BundleError
from .bundle_hs import load_hs_bundle
from .hs_taus import load as load_taus
from .scorer_hs import HSTraceScorer, adapt_v2_nodes

log = get_logger("app_hs")

CONTRACT_V3 = "brain-api/v3"
SPEC_VERSION = "v6828t3"
_V2_CHANGEPRONE_COL = 42          # v2's 49-col row carries changeprone at 42; the HS lineage has none
_V2_REREQ = slice(16, 22)         # v3 decided cols 16-21

ExtClass = Literal["gml", "ps1", "md", "txt", "json", "py", "png", "log", "yy", "jsonl", "sh",
                   "diff", "html", "yyp", "patch", "js", "ts", "tsx", "rs", "go", "java", "c", "h",
                   "cpp", "yaml", "yml", "toml", "rb", "cs", "css", "other", "none"]


class V3Node(BaseModel):
    model_config = ConfigDict(extra="forbid")
    text: Text2000
    cmd: str
    head: Text240
    struct: list[float] = Field(min_length=27, max_length=27)   # node_struct_with_type + rereq
    step: int = Field(ge=0)
    kind: Literal["read", "grep", "other", "reasoning", "asst"]
    tokens: int = Field(ge=1)
    ext: ExtClass
    file_id: str | None = Field(default=None, pattern=_HEX16)
    lo: int | None = None
    hi: int | None = None
    cmd_id: str | None = Field(default=None, pattern=_HEX16)
    head_id: str | None = Field(default=None, pattern=_HEX16)


class ScoreTraceV3Request(BaseModel):
    model_config = ConfigDict(extra="forbid")
    contract: Literal["brain-api/v3"]
    conv_id: str = Field(pattern=_IDENT)
    checkpoint_id: str = Field(pattern=_HEX64)
    cur_step: int = Field(ge=0)
    target_cov: str | None = Field(default=None, pattern=r"^0\.\d{2}$")
    nodes: list[V3Node]
    task_text: Text2000
    sys_text: Text2000 | None = None
    mask: list[int]
    decided_struct: list[list[float]]
    edges_supersession: list[tuple[int, int]]
    gf: Gf4 | None = None


ScoreTraceHSBody = Annotated[Union[ScoreTraceV3Request, ScoreTraceV2Request],
                             Field(discriminator="contract")]


def v2_decided_to_v3(row49, rereq_row) -> list[float]:
    """49-col v2 readout row -> the 104-col v3 layout (struct 0-15 | rereq 16-21 | read cols
    22-71 zero | age..scout 72-101 | dupcos 102-103 zero; the brain fills read cols + dupcos)."""
    r = list(row49)
    if len(r) != 49:
        raise ValueError("v2 decided_struct rows must be 49 wide")
    del r[_V2_CHANGEPRONE_COL]                    # -> 48: sf16 | age..centrality 26 | scout 4 | dupcos 2
    return r[:16] + [float(x) for x in rereq_row] + [0.0] * 50 + r[16:46] + [0.0, 0.0]


def fold_kind_tau(scores_q: list[int], kinds: list[str], tau_q: dict[str, int], t: int) -> list[int]:
    out = []
    for q, k in zip(scores_q, kinds):
        if k not in tau_q:
            out.append(GRID)                      # asst: no tau -> never cut
        else:
            out.append(max(0, min(GRID, q - tau_q[k] + t)))
    return out


def create_hs_app() -> FastAPI:
    bundle = load_hs_bundle()        # self-validating load assert (378/378, widths 106/104)
    taus = load_taus(bundle.ckpt_path)   # refuses to start without a per-kind tau table
    scorer = HSTraceScorer(bundle)
    tn = os.environ.get("PARSEC_TORCH_THREADS", "").strip()
    if tn.isdigit() and int(tn) > 0:
        import torch
        torch.set_num_threads(int(tn))
    try:
        scorer.embed(["parsec embedder warm-up"])
    except Exception as e:  # noqa: BLE001 — warm-up must never block startup
        log.warning("embedder warm-up failed (%s) — deferred to first request", type(e).__name__)
    tau_q = taus.tau_q
    tau_cov = f"{taus.coverage:.2f}"
    rules = _load_rules()
    rule_defaults = [{"eid": r["eid"], "text": r["text"]} for r in rules
                     if r.get("status") in _RULE_DEFAULT_STATUS]
    lock = threading.Lock()
    key = os.environ.get("PARSEC_BRAIN_KEY", "")
    platform_url = os.environ.get("PARSEC_PLATFORM_URL", "").strip()
    encoder = os.environ.get("ENCGNN_MODEL", "")
    log.info("HS brain ready ckpt=%s sha8=%s taus=%s cov=%s encoder=%s",
             os.path.basename(bundle.ckpt_path), bundle.checkpoint_id[:8], tau_q, tau_cov, encoder)

    app = FastAPI(title="parsec-brain-hs", version="0.1.0")
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

    def _guard(req_checkpoint_id: str) -> None:
        if req_checkpoint_id != bundle.checkpoint_id:
            raise HTTPException(status_code=409, detail={
                "error": "checkpoint_id mismatch", "server_checkpoint_id": bundle.checkpoint_id})

    def _check_trace(nodes, mask, decided_struct, edges, width):
        n = len(nodes)
        if any(j < 0 or j >= n for j in mask):
            raise HTTPException(status_code=422, detail=f"mask out of range for {n} nodes")
        if len(decided_struct) != len(mask):
            raise HTTPException(status_code=422,
                                detail="decided_struct must carry one row per mask entry")
        if any(len(r) != width for r in decided_struct):
            raise HTTPException(status_code=422, detail=f"decided_struct rows must be {width} wide")
        if any(not (0 <= a < n and 0 <= b < n) for (a, b) in edges):
            raise HTTPException(status_code=422,
                                detail=f"edges_supersession out of range for {n} nodes")

    @app.middleware("http")
    async def _count(request: Request, call_next):
        if request.url.path != "/health":
            COUNTERS["requests"] += 1
        return await call_next(request)

    @app.get("/health")
    def health():
        return {"status": "ok", "fail_opens": COUNTERS["fail_opens"],
                "requests": COUNTERS["requests"]}

    @app.get("/v1/bundle")
    def bundle_info(request: Request):
        _auth(request)
        return {
            "contract": CONTRACT_V3,
            "contracts": [CONTRACT_V3, CONTRACT_V2],
            "checkpoint_id": bundle.checkpoint_id,
            "read_struct": bundle.read_struct,
            "node_struct": bundle.node_struct,
            "spec_version": SPEC_VERSION,
            "tau_q_by_kind": tau_q,
            "tau_coverage": tau_cov,
            "tau_q": tau_q["read"],                  # v2 clients: the fold anchor T (see module doc)
            "encoder": encoder,
            "grid": GRID,
            "accept_gzip": True,
            "neighbors": False,
            "doom": {"gf": bundle.doom_gf, "served": bundle.doom_gf > 0},
            "heads": ["curator", "tool", "rule"],
            "flags": _flags_hs.snapshot(),
        }

    def _trace_v3(req: ScoreTraceV3Request) -> dict:
        _guard(req.checkpoint_id)
        _check_trace(req.nodes, req.mask, req.decided_struct, req.edges_supersession,
                     bundle.read_struct)
        n = len(req.nodes)
        if not req.nodes or not req.mask:
            return {"scores_q": [GRID] * n, "tau_q_by_kind": tau_q,
                    "checkpoint_id": bundle.checkpoint_id,
                    "timings_ms": {"embed": 0.0, "forward": 0.0}}
        with lock:
            sc, timings, doom = scorer.score_trace(req.nodes, req.task_text, req.sys_text,
                                                   req.mask, req.decided_struct,
                                                   req.edges_supersession, gf=req.gf)
        out = {"scores_q": [_q(s) for s in sc], "tau_q_by_kind": tau_q,
               "checkpoint_id": bundle.checkpoint_id, "timings_ms": timings}
        if doom is not None:
            out["doom_q"] = _q(doom)
        return out

    def _trace_v2(req: ScoreTraceV2Request) -> dict:
        _guard(req.checkpoint_id)
        _check_trace(req.nodes, req.mask, req.decided_struct, req.edges_supersession, 49)
        nodes = adapt_v2_nodes(req.nodes)
        ds = [v2_decided_to_v3(r, nodes[j].struct[21:27]) for r, j in zip(req.decided_struct,
                                                                           req.mask)]
        t = tau_q["read"]
        n = len(nodes)
        if not nodes or not req.mask:
            return {"scores_q": [GRID] * n, "tau_q": t, "checkpoint_id": bundle.checkpoint_id,
                    "timings_ms": {"embed": 0.0, "forward": 0.0}}
        with lock:
            sc, timings, doom = scorer.score_trace(nodes, req.task_text, req.sys_text, req.mask,
                                                   ds, req.edges_supersession, gf=req.gf)
        q = [_q(s) for s in sc]
        maskset = set(req.mask)
        folded = fold_kind_tau(q, [x.kind for x in nodes], tau_q, t)
        out = {"scores_q": [folded[i] if i in maskset else GRID for i in range(n)],
               "tau_q": t, "checkpoint_id": bundle.checkpoint_id, "timings_ms": timings}
        if doom is not None:
            out["doom_q"] = _q(doom)
        return out

    @app.post("/v1/score/trace")
    def score_trace(req: ScoreTraceHSBody, request: Request):
        _auth(request)
        v3 = isinstance(req, ScoreTraceV3Request)
        t0 = time.perf_counter()
        out = _trace_v3(req) if v3 else _trace_v2(req)
        log.info("score/trace contract=%s conv=%s n_nodes=%d n_mask=%d wall_ms=%.1f status=200",
                 "v3" if v3 else "v2", conv_sha8(req.conv_id), len(req.nodes), len(req.mask),
                 (time.perf_counter() - t0) * 1000.0)
        return out

    @app.post("/v1/score/tools")
    def score_tools(req: ScoreToolsV2Request, request: Request):
        _auth(request)
        _guard(req.checkpoint_id)
        if not req.nodes:
            raise HTTPException(status_code=422, detail="nodes must be non-empty (mirror the "
                                                        "step-0 task-chunk fallback client-side)")
        if not req.tools:
            return {"names": [], "scores_q": [], "tokens": [],
                    "checkpoint_id": bundle.checkpoint_id}
        with lock:
            try:
                sg, forward_ms = scorer.score_tools(adapt_v2_nodes(req.nodes), req.task_text,
                                                    req.sys_text, [t.schema_text for t in req.tools])
            except Exception as e:
                log.debug("HS tool scoring exception detail", exc_info=True)
                count_fail_open(log, f"HS tool scoring failed ({type(e).__name__}) -> fail-open")
                return {"names": [], "scores_q": [], "tokens": [],
                        "checkpoint_id": bundle.checkpoint_id}
        log.info("score/tools contract=v2 conv=%s n_tools=%d forward_ms=%s status=200",
                 conv_sha8(req.conv_id), len(req.tools), forward_ms)
        return {"names": [t.name for t in req.tools], "scores_q": [_q(s) for s in sg],
                "tokens": [int(t.tokens) for t in req.tools],
                "checkpoint_id": bundle.checkpoint_id}

    @app.post("/v1/score/rules")
    def score_rules(req: ScoreRulesV2Request, request: Request):
        """Rule head — tau_hint_q is advisory only (bench calibration owns the firing tau)."""
        _auth(request)
        _guard(req.checkpoint_id)
        n = len(req.nodes)
        if not req.nodes:
            raise HTTPException(status_code=422, detail="nodes must be non-empty")
        if any(not (0 <= a < n and 0 <= b < n) for (a, b) in req.edges_supersession):
            raise HTTPException(status_code=422,
                                detail=f"edges_supersession out of range for {n} nodes")
        cands = rule_defaults
        scores: dict[str, float] = {}
        if cands:
            with lock:
                try:
                    sc, _ = scorer.score_rules(adapt_v2_nodes(req.nodes), req.task_text,
                                               req.sys_text, req.edges_supersession, req.cur_step,
                                               [r["text"] for r in cands])
                    scores = {cands[k]["eid"]: float(sc[k]) for k in range(len(sc))}
                except Exception as e:
                    log.debug("HS rule scoring exception detail", exc_info=True)
                    count_fail_open(log, f"HS rule scoring failed ({type(e).__name__}) "
                                         f"-> fail-open")
        by_eid = {r["eid"]: r for r in cands}
        sq = {eid: _q(s) for eid, s in scores.items()}
        return {"scores_q": sq,
                "rules": [{"eid": eid, "text": by_eid[eid]["text"], "p_q": q,
                           "fire_step": int(req.cur_step)} for eid, q in sq.items()],
                "tau_hint_q": RULE_TAU_HINT_Q, "checkpoint_id": bundle.checkpoint_id,
                "description": RULE_TAU_DESCRIPTION}

    @app.post("/v1/neighbors")
    def neighbors(req: NeighborsV2Request, request: Request):
        _auth(request)
        _guard(req.checkpoint_id)
        return {"nbr_cost_median": None, "nbr_count": 0, "neighbors_active": False,
                "checkpoint_id": bundle.checkpoint_id}

    return app


def _load_rules() -> list[dict]:
    """rules.json roster (server-owned rule text), the same file and checks as bundle.load_bundle."""
    import json
    from pathlib import Path
    path = Path(os.environ.get("PARSEC_RULES_JSON", str(_PKG_MODELS / "rules.json")))
    try:
        rules = json.loads(path.read_text())
    except Exception as e:
        raise BundleError(f"rules roster unreadable at {path}: {e}") from None
    if not isinstance(rules, list) or not all(isinstance(r, dict) and r.get("eid") and r.get("text")
                                              for r in rules):
        raise BundleError(f"rules roster malformed at {path}: expected [{{eid, text, ...}}]")
    return rules
